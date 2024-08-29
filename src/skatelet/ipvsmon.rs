use std::collections::HashSet;
use crate::skate::exec_cmd;
use crate::util::NamespacedName;
use anyhow::anyhow;
use clap::Args;
use handlebars::Handlebars;
use k8s_openapi::api::core::v1::Service;
use log::info;
use serde_json::json;
use std::error::Error;
use std::fs::OpenOptions;
use std::io::prelude::*;
use std::net::ToSocketAddrs;
use std::path::Path;
use std::fs;
use std::hash::{DefaultHasher, Hash, Hasher};
use itertools::Itertools;

#[derive(Debug, Args)]
pub struct IpvsmonArgs {
    #[arg(long, long_help = "Name of the file to write keepalived config to.")]
    out: String,
    host: String,
    file: String,
}

// for each dns we get we add it to /run/skate-ipvsmon-<service>.cache
// <op> <ip> <timestamp>
// ADD 10.0.0.99 1724929735
// when the ip disappears, we add the following in the file
// DEL <ip> <newtimestamp>
// DEL 10.0.0.99 1724929767
// periodically we clean the file from entries older than 5 minutes
// ipvsmon reads the ips from the file, adding any DEL's with weight 0


pub fn ipvsmon(args: IpvsmonArgs) -> Result<(), Box<dyn Error>> {
    // args.service_name is fqn like foo.bar
    let mut manifest: Service = serde_yaml::from_str(&fs::read_to_string(args.file)?)?;
    let spec = manifest.spec.clone().unwrap_or_default();
    let name = spec.selector.unwrap_or_default().get("app.kubernetes.io/name").unwrap_or(&"default".to_string()).clone();
    if name == "" {
        return Err(anyhow!("service selector app.kubernetes.io/name is required").into());
    }
    let ns = manifest.metadata.namespace.unwrap_or("default".to_string());
    let fqn = NamespacedName { name, namespace: ns.clone() };
    manifest.metadata.namespace = Some(ns);


    let domain = format!("{}.pod.cluster.skate:80", fqn);
    // get all pod ips from dns <args.service_name>.cluster.skate
    info!("looking up ips for {}", &domain);
    let addrs: HashSet<_> = domain.to_socket_addrs().unwrap_or_default()
        .map(|addr| addr.ip().to_string()).collect();


    // hashes match and output file exists
    if !hash_changed(&addrs, &fqn.to_string())? && Path::new(&args.out).exists() {
        info!("ips haven't changed: {:?}", &addrs);
        return Ok(());
    }

    info!("ips changed, rewriting keepalived config for {} -> {:?}", &args.host, &addrs);
    // check the old ADD ips in the cache file (remove those with a DEL line)
    let deleted = terminated_list(&fqn.to_string())?;
    let last_result = lastresult_list(&fqn.to_string())?;
    let _ = lastresult_save(&fqn.to_string(), &addrs.iter().map(|i|i.clone()).collect())?;
    // remove deleted items that are in the latest result
    let deleted = deleted.iter().map(|i| i.clone()).filter(|i| !addrs.contains(i)).collect::<Vec<String>>();

    // find those that are now missing, add those to the cache file under DEL
    let missing_now: Vec<_> = last_result.difference(&addrs).map(|i| i.clone()).collect();
    let _ = terminated_add(&fqn.to_string(), &missing_now)?;

    let new: Vec<_> = addrs.difference(&last_result).map(|i| i.clone()).collect();
    info!("added: {:?}", new);
    info!("deleted: {:?}", missing_now);

    // append newly missing to deleted list
    let deleted = [deleted, missing_now].concat();


    // rewrite keepalived include file
    let mut handlebars = Handlebars::new();
    handlebars.set_strict_mode(true);

    handlebars.register_template_string("keepalived", include_str!("../resources/keepalived-service.conf")).map_err(|e| anyhow!(e).context("failed to load keepalived file"))?;

    // write config
    {
        let file = OpenOptions::new().write(true).create(true).truncate(true).open(args.out)?;
        handlebars.render_to_write("keepalived", &json!({
            "host": args.host,
            "manifest": manifest,
            "target_ips": addrs,
            "deleted_ips": deleted,
        }), file)?;
    }


    // reload keepalived
    let _ = exec_cmd("systemctl", &["reload", "keepalived"])?;
    Ok(())
}

fn hash_changed(addrs: &HashSet<String>, service_name: &str) -> Result<bool, Box<dyn Error>> {
    let mut hasher = DefaultHasher::new();
    let addrs: Vec<_> = addrs.iter().map(|i| i.clone()).sorted().collect();
    addrs.hash(&mut hasher);
    let new_hash = format!("{:x}", hasher.finish());
    let hash_file_name = format!("/run/skatelet-ipvsmon-{}.hash", service_name);

    let old_hash = fs::read_to_string(&hash_file_name).unwrap_or_default();

    let changed = old_hash != new_hash;
    if changed {
        fs::write(&hash_file_name, new_hash)?;
    }
    Ok(changed)
}

fn terminated_list_file_name(service_name: &str) -> String {
    format!("/run/skatelet-ipvsmon-{}.terminated", service_name)
}

fn last_result_file_name(service_name: &str) -> String {
    format!("/run/skatelet-ipvsmon-{}.lastresult", service_name)
}

fn lastresult_save(service_name: &str, ips: &Vec<String>) -> Result<(), Box<dyn Error>> {
    fs::write(last_result_file_name(service_name), ips.join("\n"))?;
    Ok(())
}
fn lastresult_list(service_name: &str) -> Result<HashSet<String>, Box<dyn Error>> {
    let contents = match fs::read_to_string(last_result_file_name(service_name)) {
        Ok(contents) => contents,
        Err(_) => return Ok(HashSet::new())
    };

    Ok(contents.lines().map(|i| i.to_string()).collect())
}

fn terminated_add(service_name: &str, ips: &Vec<String>) -> Result<(), Box<dyn Error>> {
    let mut file = OpenOptions::new().write(true).append(true).create(true).open(terminated_list_file_name(service_name))?;
    for ip in ips {
        file.write_all(format!("{} {}\n", ip, chrono::Utc::now().timestamp()).as_bytes())?;
    }
    Ok(())
}

fn terminated_list(service_name: &str) -> Result<HashSet<String>, Box<dyn Error>> {
    let mut contents = match fs::read_to_string(terminated_list_file_name(service_name)) {
        Ok(contents) => contents,
        Err(_) => return Ok(HashSet::new())
    };

    let mut deleted = HashSet::new();

    for line in contents.lines().sorted().rev() {
        let parts: Vec<_> = line.split_whitespace().collect();
        if parts.len() != 3 {
            continue;
        }
        let op = parts[0];
        let ip = parts[1];

        if deleted.contains(ip) {
            continue;
        }
        deleted.insert(ip.to_string());
    }

    Ok(deleted)
}

fn cleanup_terminated_list(service_name: &str) -> Result<(), Box<dyn Error>> {
    let mut file = OpenOptions::new().write(true).append(true).create(true).open(terminated_list_file_name(service_name))?;
    let mut contents = String::new();
    file.read_to_string(&mut contents)?;
    let mut new_contents = String::new();
    // want DEL lines first

    let now = chrono::Utc::now().timestamp();

    let mut ip_set = HashSet::new();

    for line in contents.lines().sorted().rev() {
        let parts: Vec<_> = line.split_whitespace().collect();
        if parts.len() != 3 {
            continue;
        }
        let op = parts[0];
        let ip = parts[1];
        let ts = parts[2].parse::<i64>()?;

        if ip_set.contains(ip) {
            continue;
        }
        if op == "DEL" && now - ts > 300 {
            ip_set.insert(ip);
            continue;
        }
        new_contents.push_str(line);
        new_contents.push_str("\n");
    }

    file.seek(std::io::SeekFrom::Start(0))?;
    file.write_all(new_contents.as_bytes())?;
    Ok(())
}
