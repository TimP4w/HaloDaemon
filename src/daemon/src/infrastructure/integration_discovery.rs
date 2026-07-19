// SPDX-License-Identifier: GPL-3.0-or-later

use std::collections::HashMap;
use std::net::UdpSocket;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use mdns_sd::{ServiceDaemon, ServiceEvent};
use serde_json::{json, Value};

use crate::domain::plugin::manifest::IntegrationDiscoveryConfig;

pub async fn scan(config: IntegrationDiscoveryConfig) -> Result<Value> {
    let mdns_config = config.clone();
    let mdns_task = tokio::task::spawn_blocking(move || scan_mdns(&mdns_config.mdns));
    let ssdp_task = tokio::task::spawn_blocking(move || scan_ssdp(&config.ssdp));
    let (mdns, ssdp) = tokio::join!(mdns_task, ssdp_task);
    let mdns = mdns.context("mDNS discovery task panicked")??;
    let ssdp = ssdp.context("SSDP discovery task panicked")??;
    Ok(json!({ "mdns": mdns, "ssdp": ssdp }))
}

fn scan_mdns(services: &[String]) -> Result<Vec<Value>> {
    let daemon = ServiceDaemon::new().context("starting mDNS browser")?;
    let mut found = HashMap::<String, Value>::new();
    for service in services {
        let receiver = daemon
            .browse(service)
            .with_context(|| format!("browsing mDNS service '{service}'"))?;
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if let Ok(ServiceEvent::ServiceResolved(info)) =
                receiver.recv_timeout(Duration::from_millis(150))
            {
                let addresses: Vec<String> = info
                    .get_addresses()
                    .iter()
                    .map(ToString::to_string)
                    .collect();
                let txt: HashMap<String, String> = info
                    .get_properties()
                    .iter()
                    .map(|property| (property.key().to_owned(), property.val_str().to_owned()))
                    .collect();
                let id = info.get_fullname().to_owned();
                found.insert(
                    id.clone(),
                    json!({
                        "service": service,
                        "id": id,
                        "name": info.get_fullname(),
                        "host": info.get_hostname(),
                        "port": info.get_port(),
                        "addresses": addresses,
                        "txt": txt,
                    }),
                );
            }
        }
        let _ = daemon.stop_browse(service);
    }
    let _ = daemon.shutdown();
    Ok(found.into_values().collect())
}

fn scan_ssdp(targets: &[String]) -> Result<Vec<Value>> {
    if targets.is_empty() {
        return Ok(Vec::new());
    }
    let socket = UdpSocket::bind("0.0.0.0:0").context("binding SSDP discovery socket")?;
    socket
        .set_read_timeout(Some(Duration::from_millis(200)))
        .context("setting SSDP timeout")?;
    for target in targets {
        let request = format!(
            "M-SEARCH * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nMAN: \"ssdp:discover\"\r\nMX: 2\r\nST: {target}\r\n\r\n"
        );
        socket
            .send_to(request.as_bytes(), "239.255.255.250:1900")
            .with_context(|| format!("sending SSDP search for '{target}'"))?;
    }
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut found = HashMap::<String, Value>::new();
    let mut buffer = [0u8; 8192];
    while Instant::now() < deadline {
        let Ok((length, source)) = socket.recv_from(&mut buffer) else {
            continue;
        };
        let Ok(text) = std::str::from_utf8(&buffer[..length]) else {
            continue;
        };
        let headers = parse_headers(text);
        let response_target = headers.get("st").cloned().unwrap_or_default();
        if !targets.iter().any(|target| target == &response_target) {
            continue;
        }
        let location = headers.get("location").cloned().unwrap_or_default();
        let id = headers
            .get("usn")
            .cloned()
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| format!("{response_target}@{source}"));
        found.insert(
            id.clone(),
            json!({
                "service": response_target,
                "target": response_target,
                "id": id,
                "location": location,
                "source": source.to_string(),
                "server": headers.get("server").cloned().unwrap_or_default(),
            }),
        );
    }
    Ok(found.into_values().collect())
}

fn parse_headers(response: &str) -> HashMap<String, String> {
    response
        .lines()
        .skip(1)
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_owned()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssdp_headers_are_case_insensitive_and_trimmed() {
        let headers = parse_headers(
            "HTTP/1.1 200 OK\r\nLOCATION: http://192.0.2.1/device.xml\r\nUsN: uuid:one\r\n\r\n",
        );
        assert_eq!(headers["location"], "http://192.0.2.1/device.xml");
        assert_eq!(headers["usn"], "uuid:one");
    }
}
