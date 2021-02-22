/*
 * Copyright (c)2013-2020 ZeroTier, Inc.
 *
 * Use of this software is governed by the Business Source License included
 * in the LICENSE.TXT file in the project's root directory.
 *
 * Change Date: 2025-01-01
 *
 * On the date above, in accordance with the Business Source License, use
 * of this software will be governed by version 2.0 of the Apache License.
 */
/****/

use std::collections::BTreeMap;
use std::net::{IpAddr, SocketAddr};
use std::str::FromStr;
use std::sync::{Arc, Mutex, Weak};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use futures::stream::StreamExt;
use warp::{Filter, Reply};
use warp::http::{HeaderMap, Method, StatusCode};
use warp::hyper::body::Bytes;

use zerotier_core::{Buffer, Address, IpScope, Node, NodeEventHandler, NetworkId, VirtualNetworkConfigOperation, VirtualNetworkConfig, StateObjectType, MAC, Event, InetAddress, InetAddressFamily, Identity, Dictionary};

use crate::fastudpsocket::*;
use crate::{getifaddrs, ms_since_epoch};
use crate::localconfig::*;
use crate::log::Log;
use crate::network::Network;
use crate::store::Store;

const CONFIG_CHECK_INTERVAL: i64 = 5000;

#[derive(Clone)]
struct Service {
    auth_token: Arc<String>,
    log: Arc<Log>,
    _local_config: Arc<Mutex<Arc<LocalConfig>>>,
    run: Arc<AtomicBool>,
    online: Arc<AtomicBool>,
    store: Arc<Store>,
    node: Weak<Node<Service, Network>>, // weak since Node itself may hold a reference to this
}

impl NodeEventHandler<Network> for Service {
    fn virtual_network_config(&self, network_id: NetworkId, network_obj: &Arc<Network>, config_op: VirtualNetworkConfigOperation, config: Option<&VirtualNetworkConfig>) {}

    #[inline(always)]
    fn virtual_network_frame(&self, network_id: NetworkId, network_obj: &Arc<Network>, source_mac: MAC, dest_mac: MAC, ethertype: u16, vlan_id: u16, data: &[u8]) {}

    fn event(&self, event: Event, event_data: &[u8]) {
        match event {
            Event::Up => {}
            Event::Down => {
                self.run.store(false, Ordering::Relaxed);
            }
            Event::Online => {
                self.online.store(true, Ordering::Relaxed);
            }
            Event::Offline => {
                self.online.store(true, Ordering::Relaxed);
            }
            Event::Trace => {
                if !event_data.is_empty() {
                    let _ = Dictionary::new_from_bytes(event_data).map(|tm| {
                        let tm = zerotier_core::trace::TraceEvent::parse_message(&tm);
                        let _ = tm.map(|tm| {
                            self.log.log(tm.to_string());
                        });
                    });
                }
            }
            Event::UserMessage => {}
        }
    }

    #[inline(always)]
    fn state_put(&self, obj_type: StateObjectType, obj_id: &[u64], obj_data: &[u8]) -> std::io::Result<()> {
        self.store.store_object(&obj_type, obj_id, obj_data)
    }

    #[inline(always)]
    fn state_get(&self, obj_type: StateObjectType, obj_id: &[u64]) -> std::io::Result<Vec<u8>> {
        self.store.load_object(&obj_type, obj_id)
    }

    #[inline(always)]
    fn wire_packet_send(&self, local_socket: i64, sock_addr: &InetAddress, data: &[u8], packet_ttl: u32) -> i32 {
        0
    }

    fn path_check(&self, address: Address, id: &Identity, local_socket: i64, sock_addr: &InetAddress) -> bool {
        true
    }

    fn path_lookup(&self, address: Address, id: &Identity, desired_family: InetAddressFamily) -> Option<InetAddress> {
        let lc = self.local_config();
        let vc = lc.virtual_.get(&address);
        vc.map_or(None, |c: &LocalConfigVirtualConfig| {
            if c.try_.is_empty() {
                None
            } else {
                let t = c.try_.get((zerotier_core::random() as usize) % c.try_.len());
                t.map_or(None, |v: &InetAddress| {
                    Some(v.clone())
                })
            }
        })
    }
}

impl Service {
    #[inline(always)]
    fn web_api_status(&self, remote: Option<SocketAddr>, method: Method, headers: HeaderMap, post_data: Bytes) -> Box<dyn Reply> {
        Box::new(StatusCode::BAD_REQUEST)
    }

    #[inline(always)]
    fn web_api_network(&self, network_str: String, remote: Option<SocketAddr>, method: Method, headers: HeaderMap, post_data: Bytes) -> Box<dyn Reply> {
        Box::new(StatusCode::BAD_REQUEST)
    }

    #[inline(always)]
    fn web_api_peer(&self, peer_str: String, remote: Option<SocketAddr>, method: Method, headers: HeaderMap, post_data: Bytes) -> Box<dyn Reply> {
        Box::new(StatusCode::BAD_REQUEST)
    }

    #[inline(always)]
    fn local_config(&self) -> Arc<LocalConfig> {
        self._local_config.lock().unwrap().clone()
    }

    #[inline(always)]
    fn set_local_config(&self, new_lc: LocalConfig) {
        *(self._local_config.lock().unwrap()) = Arc::new(new_lc);
    }
}

pub(crate) fn run(store: &Arc<Store>, auth_token: Option<String>) -> i32 {
    let mut process_exit_value: i32 = 0;

    let init_local_config = Arc::new(store.read_local_conf(false).unwrap_or_else(|_| { LocalConfig::default() }));

    let log = Arc::new(Log::new(
        if init_local_config.settings.log_path.as_ref().is_some() {
            init_local_config.settings.log_path.as_ref().unwrap().as_str()
        } else {
            store.default_log_path.to_str().unwrap()
        },
        init_local_config.settings.log_size_max,
        init_local_config.settings.log_to_stderr,
        "",
    ));

    // Generate authtoken.secret from secure random bytes if not already set.
    let auth_token = auth_token.unwrap_or_else(|| {
        let mut rb = [0_u8; 64];
        unsafe { crate::osdep::getSecureRandom(rb.as_mut_ptr().cast(), 64) };
        let mut t = String::new();
        t.reserve(64);
        for b in rb.iter() {
            if *b > 127_u8 {
                t.push((65 + (*b % 26)) as char); // A..Z
            } else {
                t.push((97 + (*b % 26)) as char); // a..z
            }
        }
        if store.write_authtoken_secret(t.as_str()).is_err() {
            t.clear();
        }
        t
    });
    if auth_token.is_empty() {
        l!(log, "FATAL: unable to write authtoken.secret to '{}'", store.base_path.to_str().unwrap());
        return 1;
    }
    let auth_token = Arc::new(auth_token);

    // From this point on we're in tokio / async.
    let tokio_rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
    tokio_rt.block_on(async {
        let mut udp_sockets: BTreeMap<InetAddress, FastUDPSocket> = BTreeMap::new();
        let (mut interrupt_tx, mut interrupt_rx) = futures::channel::mpsc::channel::<()>(1);

        // Create clonable implementation of NodeEventHandler and local web API endpoints.
        let mut service = Service {
            auth_token: auth_token.clone(),
            log: log.clone(),
            _local_config: Arc::new(Mutex::new(init_local_config)),
            run: Arc::new(AtomicBool::new(true)),
            online: Arc::new(AtomicBool::new(false)),
            store: store.clone(),
            node: Weak::new(),
        };

        // Create instance of Node which will call Service on events.
        let node = Node::new(service.clone(), ms_since_epoch());
        if node.is_err() {
            process_exit_value = 1;
            l!(log, "FATAL: error initializing node: {}", node.err().unwrap().to_str());
            return;
        }
        let node = Arc::new(node.ok().unwrap());

        service.node = Arc::downgrade(&node);
        let service = service; // make immutable after setting node

        // The outer loop runs for as long as the service runs. It repeatedly restarts
        // the inner loop, which can exit if it needs to be restarted. This is the case
        // if a major configuration change occurs.
        let mut loop_delay = zerotier_core::NODE_BACKGROUND_TASKS_MAX_INTERVAL;
        loop {
            let mut local_config = service.local_config();

            let (mut shutdown_tx, mut shutdown_rx) = futures::channel::oneshot::channel();
            let warp_server;
            {
                let s0 = service.clone();
                let s1 = service.clone();
                let s2 = service.clone();
                warp_server = warp::serve(warp::any()
                    .and(warp::path::end().map(|| { warp::reply::with_status("404", StatusCode::NOT_FOUND) })
                        .or(warp::path("status")
                            .and(warp::addr::remote())
                            .and(warp::method())
                            .and(warp::header::headers_cloned())
                            .and(warp::body::content_length_limit(1048576))
                            .and(warp::body::bytes())
                            .map(move |remote: Option<SocketAddr>, method: Method, headers: HeaderMap, post_data: Bytes| { s0.web_api_status(remote, method, headers, post_data) }))
                        .or(warp::path!("network" / String)
                            .and(warp::addr::remote())
                            .and(warp::method())
                            .and(warp::header::headers_cloned())
                            .and(warp::body::content_length_limit(1048576))
                            .and(warp::body::bytes())
                            .map(move |network_str: String, remote: Option<SocketAddr>, method: Method, headers: HeaderMap, post_data: Bytes| { s1.web_api_network(network_str, remote, method, headers, post_data) }))
                        .or(warp::path!("peer" / String)
                            .and(warp::addr::remote())
                            .and(warp::method())
                            .and(warp::header::headers_cloned())
                            .and(warp::body::content_length_limit(1048576))
                            .and(warp::body::bytes())
                            .map(move |peer_str: String, remote: Option<SocketAddr>, method: Method, headers: HeaderMap, post_data: Bytes| { s2.web_api_peer(peer_str, remote, method, headers, post_data) }))
                    )
                ).try_bind_with_graceful_shutdown((IpAddr::from([127_u8, 0_u8, 0_u8, 1_u8]), local_config.settings.primary_port), async { let _ = shutdown_rx.await; });
            }
            if warp_server.is_err() {
                l!(log, "ERROR: local API http server failed to bind to port {} or failed to start: {}", local_config.settings.primary_port, warp_server.err().unwrap().to_string());
                break;
            }
            let warp_server = tokio_rt.spawn(warp_server.unwrap().1);

            // Write zerotier.port which is used by the CLI to know how to reach the HTTP API.
            store.write_port(local_config.settings.primary_port);

            // The inner loop runs the web server in the "background" (async) while periodically
            // scanning for significant configuration changes. Some major changes may require
            // the inner loop to exit and be restarted.
            let mut last_checked_config: i64 = 0;
            loop {
                let loop_start = ms_since_epoch();
                let mut now: i64 = 0;

                // Wait for (1) loop delay elapsed, (2) a signal to interrupt delay now, or
                // (3) an external signal to exit.
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_millis(loop_delay)) => {
                        now = ms_since_epoch();
                        let actual_delay = now - loop_start;
                        if actual_delay > ((loop_delay as i64) * 4_i64) {
                            l!(log, "likely sleep/wake detected, reestablishing links...");
                            // TODO: handle likely sleep/wake or other system interruption
                        }
                    },
                    _ = interrupt_rx.next() => {
                        now = ms_since_epoch();
                    },
                    _ = tokio::signal::ctrl_c() => {
                        l!(log, "exit signal received, shutting down...");
                        service.run.store(false, Ordering::Relaxed);
                        break;
                    }
                }

                // Check every CONFIG_CHECK_INTERVAL for changes to either the system configuration
                // or the node's local configuration and take actions as needed.
                if (now - last_checked_config) >= CONFIG_CHECK_INTERVAL {
                    last_checked_config = now;

                    // Check for changes to local.conf.
                    let new_config = store.read_local_conf(true);
                    if new_config.is_ok() {
                        service.set_local_config(new_config.unwrap());
                    }

                    // Check for and handle configuration changes, some of which require inner loop restart.
                    let next_local_config = service.local_config();
                    if local_config.settings.primary_port != next_local_config.settings.primary_port {
                        break;
                    }
                    if local_config.settings.log_size_max != next_local_config.settings.log_size_max {
                        log.set_max_size(next_local_config.settings.log_size_max);
                    }
                    if local_config.settings.log_to_stderr != next_local_config.settings.log_to_stderr {
                        log.set_log_to_stderr(next_local_config.settings.log_to_stderr);
                    }
                    local_config = next_local_config;

                    // Enumerate all useful addresses bound to interfaces on the system.
                    let mut system_addrs: BTreeMap<InetAddress, String> = BTreeMap::new();
                    getifaddrs::for_each_address(|addr: &InetAddress, dev: &str| {
                        match addr.ip_scope() {
                            IpScope::Global | IpScope::Private | IpScope::PseudoPrivate | IpScope::Shared => {
                                if !local_config.settings.is_interface_blacklisted(dev) {
                                    let mut a = addr.clone();
                                    a.set_port(local_config.settings.primary_port);
                                    system_addrs.insert(a, String::from(dev));
                                    if local_config.settings.secondary_port.is_some() {
                                        let mut a = addr.clone();
                                        a.set_port(local_config.settings.secondary_port.unwrap());
                                        system_addrs.insert(a, String::from(dev));
                                    }
                                }
                            }
                            _ => {}
                        }
                    });

                    // Drop bound sockets that are no longer valid or are now blacklisted.
                    let mut udp_sockets_to_close: Vec<InetAddress> = Vec::new();
                    for sock in udp_sockets.iter() {
                        if !system_addrs.contains_key(sock.0) {
                            udp_sockets_to_close.push(sock.0.clone());
                        }
                    }
                    for k in udp_sockets_to_close.iter() {
                        udp_sockets.remove(k);
                    }

                    // Create sockets for unbound addresses.
                    for addr in system_addrs.iter() {
                        if !udp_sockets.contains_key(addr.0) {
                            let s = FastUDPSocket::new(addr.1.as_str(), addr.0, |raw_socket: &FastUDPRawOsSocket, from_address: &InetAddress, data: Buffer| {
                                // TODO: incoming packet handler
                            });
                            if s.is_ok() {
                                udp_sockets.insert(addr.0.clone(), s.unwrap());
                            }
                        }
                    }

                    // Determine if primary and secondary port (if secondary enabled) failed to
                    // bind to any interface.
                    let mut primary_port_bind_failure = true;
                    let mut secondary_port_bind_failure = local_config.settings.secondary_port.is_some();
                    for s in udp_sockets.iter() {
                        if s.0.port() == local_config.settings.primary_port {
                            primary_port_bind_failure = false;
                            if !secondary_port_bind_failure {
                                break;
                            }
                        }
                        if s.0.port() == local_config.settings.secondary_port.unwrap() {
                            secondary_port_bind_failure = false;
                            if !primary_port_bind_failure {
                                break;
                            }
                        }
                    }

                    if primary_port_bind_failure {
                        if local_config.settings.auto_port_search {
                            // TODO: port hunting
                        } else {
                            l!(log, "primary port {} failed to bind, waiting and trying again...", local_config.settings.primary_port);
                            break;
                        }
                    }

                    if secondary_port_bind_failure {
                        if local_config.settings.auto_port_search {
                            // TODO: port hunting
                        } else {
                            l!(log, "secondary port {} failed to bind (non-fatal, will try again)", local_config.settings.secondary_port.unwrap_or(0));
                        }
                    }
                }

                // Check to make sure nothing outside this code turned off the run flag.
                if !service.run.load(Ordering::Relaxed) {
                    break;
                }

                // Run background task handler in ZeroTier core.
                loop_delay = node.process_background_tasks(now);
            }

            // Gracefully shut down the local web server.
            let _ = shutdown_tx.send(());
            let _ = warp_server.await;

            // Sleep for a brief period of time to prevent thrashing if some invalid
            // state is hit that causes the inner loop to keep breaking.
            if !service.run.load(Ordering::Relaxed) {
                break;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
            if !service.run.load(Ordering::Relaxed) {
                break;
            }
        }
    });

    process_exit_value
}
