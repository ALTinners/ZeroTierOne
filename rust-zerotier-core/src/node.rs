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

use std::collections::hash_map::HashMap;
use std::intrinsics::copy_nonoverlapping;
use std::mem::{MaybeUninit, transmute};
use std::os::raw::{c_int, c_uint, c_ulong, c_void};
use std::ptr::{null_mut, slice_from_raw_parts};
use std::sync::*;

use num_traits::FromPrimitive;
use serde::{Deserialize, Serialize};

use crate::*;
use crate::capi as ztcore;

/// Maximum delay between calls to run_background_tasks()
pub const NODE_BACKGROUND_TASKS_MAX_INTERVAL: u64 = 250;

#[derive(FromPrimitive, ToPrimitive, PartialEq, Eq)]
pub enum Event {
    Up = ztcore::ZT_Event_ZT_EVENT_UP as isize,
    Offline = ztcore::ZT_Event_ZT_EVENT_OFFLINE as isize,
    Online = ztcore::ZT_Event_ZT_EVENT_ONLINE as isize,
    Down = ztcore::ZT_Event_ZT_EVENT_DOWN as isize,
    Trace = ztcore::ZT_Event_ZT_EVENT_TRACE as isize,
    UserMessage = ztcore::ZT_Event_ZT_EVENT_USER_MESSAGE as isize,
}

#[derive(FromPrimitive, ToPrimitive, PartialEq, Eq)]
pub enum StateObjectType {
    IdentityPublic = ztcore::ZT_StateObjectType_ZT_STATE_OBJECT_IDENTITY_PUBLIC as isize,
    IdentitySecret = ztcore::ZT_StateObjectType_ZT_STATE_OBJECT_IDENTITY_SECRET as isize,
    Locator = ztcore::ZT_StateObjectType_ZT_STATE_OBJECT_LOCATOR as isize,
    Peer = ztcore::ZT_StateObjectType_ZT_STATE_OBJECT_PEER as isize,
    NetworkConfig = ztcore::ZT_StateObjectType_ZT_STATE_OBJECT_NETWORK_CONFIG as isize,
    TrustStore = ztcore::ZT_StateObjectType_ZT_STATE_OBJECT_TRUST_STORE as isize,
    Certificate = ztcore::ZT_StateObjectType_ZT_STATE_OBJECT_CERT as isize
}

/// The status of a ZeroTier node.
#[derive(Serialize, Deserialize)]
pub struct NodeStatus {
    pub address: Address,
    pub identity: Identity,
    #[serde(rename = "publicIdentity")]
    pub public_identity: String,
    #[serde(rename = "secretIdentity")]
    pub secret_identity: String,
    pub online: bool,
}

/// An event handler that receives events, frames, and packets from the core.
/// Note that these handlers can be called concurrently from any thread and
/// must be thread safe.
pub trait NodeEventHandler<N: 'static> {
    /// Called when a configuration change or update should be applied to a network.
    fn virtual_network_config(&self, network_id: NetworkId, network_obj: &Arc<N>, config_op: VirtualNetworkConfigOperation, config: Option<&VirtualNetworkConfig>);

    /// Called when a frame should be injected into the virtual network (physical -> virtual).
    fn virtual_network_frame(&self, network_id: NetworkId, network_obj: &Arc<N>, source_mac: MAC, dest_mac: MAC, ethertype: u16, vlan_id: u16, data: &[u8]);

    /// Called when a core ZeroTier event occurs.
    fn event(&self, event: Event, event_data: &[u8]);

    /// Called to store an object into the object store.
    fn state_put(&self, obj_type: StateObjectType, obj_id: &[u64], obj_data: &[u8]) -> std::io::Result<()>;

    /// Called to retrieve an object from the object store.
    fn state_get(&self, obj_type: StateObjectType, obj_id: &[u64]) -> std::io::Result<Vec<u8>>;

    /// Called to send a packet over the physical network (virtual -> physical).
    fn wire_packet_send(&self, local_socket: i64, sock_addr: &InetAddress, data: &[u8], packet_ttl: u32) -> i32;

    /// Called to check and see if a physical address should be used for ZeroTier traffic.
    fn path_check(&self, address: Address, id: &Identity, local_socket: i64, sock_addr: &InetAddress) -> bool;

    /// Called to look up a path to a known node, allowing out of band lookup methods for physical paths to nodes.
    fn path_lookup(&self, address: Address, id: &Identity, desired_family: InetAddressFamily) -> Option<InetAddress>;
}

/// An instance of the ZeroTier core.
/// This is templated on the actual implementation of NodeEventHandler for performance reasons,
/// as it avoids an extra indirect function call.
#[allow(non_snake_case)]
pub struct Node<T: NodeEventHandler<N> + Sync + Send + Clone + 'static, N: 'static> {
    event_handler: T,
    capi: *mut ztcore::ZT_Node,
    now: PortableAtomicI64,
    networks_by_id: Mutex<HashMap<u64, *mut Arc<N>>> // pointer to an Arc<> is a raw value created from Box<Arc<N>>
}

//////////////////////////////////////////////////////////////////////////////////////////////////////////////////////

macro_rules! node_from_raw_ptr {
    ($uptr:ident) => {
        unsafe {
            let ntmp: *const Node<T, N> = $uptr.cast::<Node<T, N>>();
            let ntmp: &Node<T, N> = &*ntmp;
            ntmp
        }
    }
}

extern "C" fn zt_virtual_network_config_function<T: NodeEventHandler<N> + Sync + Send + Clone + 'static, N: 'static>(
    _: *mut ztcore::ZT_Node,
    uptr: *mut c_void,
    _: *mut c_void,
    nwid: u64,
    nptr: *mut *mut c_void,
    op: ztcore::ZT_VirtualNetworkConfigOperation,
    conf: *const ztcore::ZT_VirtualNetworkConfig,
) {
    let op2 = VirtualNetworkConfigOperation::from_i32(op as i32);
    if op2.is_some() {
        let op2 = op2.unwrap();
        let n = node_from_raw_ptr!(uptr);
        let network_obj = unsafe { &*((*nptr).cast::<Arc<N>>()) };
        if conf.is_null() {
            n.event_handler.virtual_network_config(NetworkId(nwid), network_obj, op2, None);
        } else {
            let conf2 = unsafe { VirtualNetworkConfig::new_from_capi(&*conf) };
            n.event_handler.virtual_network_config(NetworkId(nwid), network_obj, op2, Some(&conf2));
        }
    }
}

extern "C" fn zt_virtual_network_frame_function<T: NodeEventHandler<N> + Sync + Send + Clone + 'static, N: 'static>(
    _: *mut ztcore::ZT_Node,
    uptr: *mut c_void,
    _: *mut c_void,
    nwid: u64,
    nptr: *mut *mut c_void,
    source_mac: u64,
    dest_mac: u64,
    ethertype: c_uint,
    vlan_id: c_uint,
    data: *const c_void,
    data_size: c_uint,
) {
    if !nptr.is_null() {
        let n = node_from_raw_ptr!(uptr);
        n.event_handler.virtual_network_frame(
            NetworkId(nwid),
            unsafe { &*((*nptr).cast::<Arc<N>>()) },
            MAC(source_mac),
            MAC(dest_mac),
            ethertype as u16,
            vlan_id as u16,
            unsafe { &*slice_from_raw_parts(data.cast::<u8>(), data_size as usize) });
    }
}

extern "C" fn zt_event_callback<T: NodeEventHandler<N> + Sync + Send + Clone + 'static, N: 'static>(
    _: *mut ztcore::ZT_Node,
    uptr: *mut c_void,
    _: *mut c_void,
    ev: ztcore::ZT_Event,
    data: *const c_void,
    data_size: c_uint
) {
    let ev2 = Event::from_i32(ev as i32);
    if ev2.is_some() {
        let ev2 = ev2.unwrap();
        let n = node_from_raw_ptr!(uptr);
        if data.is_null() {
            n.event_handler.event(ev2, &[0_u8; 0]);
        } else {
            let data2 = unsafe { &*slice_from_raw_parts(data.cast::<u8>(), data_size as usize) };
            n.event_handler.event(ev2, data2);
        }
    }
}

extern "C" fn zt_state_put_function<T: NodeEventHandler<N> + Sync + Send + Clone + 'static, N: 'static>(
    _: *mut ztcore::ZT_Node,
    uptr: *mut c_void,
    _: *mut c_void,
    obj_type: ztcore::ZT_StateObjectType,
    obj_id: *const u64,
    obj_id_len: c_uint,
    obj_data: *const c_void,
    obj_data_len: c_int,
) {
    let obj_type2 = StateObjectType::from_i32(obj_type as i32);
    if obj_type2.is_some() {
        let obj_type2 = obj_type2.unwrap();
        let n = node_from_raw_ptr!(uptr);
        let obj_id2 = unsafe { &*slice_from_raw_parts(obj_id, obj_id_len as usize) };
        let obj_data2 = unsafe { &*slice_from_raw_parts(obj_data.cast::<u8>(), obj_data_len as usize) };
        let _ = n.event_handler.state_put(obj_type2, obj_id2, obj_data2);
    }
}

extern "C" fn zt_state_get_function<T: NodeEventHandler<N> + Sync + Send + Clone + 'static, N: 'static>(
    _: *mut ztcore::ZT_Node,
    uptr: *mut c_void,
    _: *mut c_void,
    obj_type: ztcore::ZT_StateObjectType,
    obj_id: *const u64,
    obj_id_len: c_uint,
    obj_data: *mut *mut c_void,
    obj_data_free_function: *mut *mut c_void,
) -> c_int {
    if obj_data.is_null() || obj_data_free_function.is_null() {
        return -1;
    }
    unsafe {
        *obj_data = null_mut();
        *obj_data_free_function = transmute(ztcore::free as *const ());
    }

    let obj_type2 = StateObjectType::from_i32(obj_type as i32);
    if obj_type2.is_some() {
        let obj_type2 = obj_type2.unwrap();
        let n = node_from_raw_ptr!(uptr);
        let obj_id2 = unsafe { &*slice_from_raw_parts(obj_id, obj_id_len as usize) };
        let obj_data_result = n.event_handler.state_get(obj_type2, obj_id2);
        if obj_data_result.is_ok() {
            let obj_data_result = obj_data_result.unwrap();
            if obj_data_result.len() > 0 {
                unsafe {
                    let obj_data_len: c_int = obj_data_result.len() as c_int;
                    let obj_data_raw = ztcore::malloc(obj_data_len as c_ulong);
                    if !obj_data_raw.is_null() {
                        copy_nonoverlapping(obj_data_result.as_ptr(), obj_data_raw.cast::<u8>(), obj_data_len as usize);
                        *obj_data = obj_data_raw;
                        return obj_data_len;
                    }
                }
            }
        }
    }
    return -1;
}

extern "C" fn zt_wire_packet_send_function<T: NodeEventHandler<N> + Sync + Send + Clone + 'static, N: 'static>(
    _: *mut ztcore::ZT_Node,
    uptr: *mut c_void,
    _: *mut c_void,
    local_socket: i64,
    sock_addr: *const ztcore::ZT_InetAddress,
    data: *const c_void,
    data_size: c_uint,
    packet_ttl: c_uint,
) -> c_int {
    let n = node_from_raw_ptr!(uptr);
    return n.event_handler.wire_packet_send(local_socket, InetAddress::transmute_capi(unsafe { &*sock_addr }), unsafe { &*slice_from_raw_parts(data.cast::<u8>(), data_size as usize) }, packet_ttl as u32) as c_int;
}

extern "C" fn zt_path_check_function<T: NodeEventHandler<N> + Sync + Send + Clone + 'static, N: 'static>(
    _: *mut ztcore::ZT_Node,
    uptr: *mut c_void,
    _: *mut c_void,
    address: u64,
    identity: *const ztcore::ZT_Identity,
    local_socket: i64,
    sock_addr: *const ztcore::ZT_InetAddress,
) -> c_int {
    let n = node_from_raw_ptr!(uptr);
    let id = Identity::new_from_capi(identity, false);
    if n.event_handler.path_check(Address(address), &id, local_socket, InetAddress::transmute_capi(unsafe{ &*sock_addr })) {
        return 1;
    }
    return 0;
}

extern "C" fn zt_path_lookup_function<T: NodeEventHandler<N> + Sync + Send + Clone + 'static, N: 'static>(
    _: *mut ztcore::ZT_Node,
    uptr: *mut c_void,
    _: *mut c_void,
    address: u64,
    identity: *const ztcore::ZT_Identity,
    sock_family: c_int,
    sock_addr: *mut ztcore::ZT_InetAddress,
) -> c_int {
    if sock_addr.is_null() {
        return 0;
    }
    let sock_family2: InetAddressFamily;
    unsafe {
        if sock_family == ztcore::ZT_AF_INET {
            sock_family2 = InetAddressFamily::IPv4;
        } else if sock_family == ztcore::ZT_AF_INET6 {
            sock_family2 = InetAddressFamily::IPv6;
        } else {
            return 0;
        }
    }

    let n = node_from_raw_ptr!(uptr);
    let id = Identity::new_from_capi(identity, false);
    let result = n.event_handler.path_lookup(Address(address), &id, sock_family2);
    if result.is_some() {
        let result = result.unwrap();
        let result_ptr = &result as *const InetAddress;
        unsafe {
            copy_nonoverlapping(result_ptr.cast::<ztcore::ZT_InetAddress>(), sock_addr, 1);
        }
        return 1;
    }
    return 0;
}

//////////////////////////////////////////////////////////////////////////////////////////////////////////////////////

impl<T: NodeEventHandler<N> + Sync + Send + Clone + 'static, N: 'static> Node<T, N> {
    /// Create a new Node with a given event handler.
    pub fn new(event_handler: T) -> Result<Node<T, N>, ResultCode> {
        let now = now();

        let mut n = Node {
            event_handler: event_handler.clone(),
            capi: null_mut(),
            now: PortableAtomicI64::new(now),
            networks_by_id: Mutex::new(HashMap::new())
        };

        let mut capi: *mut ztcore::ZT_Node = null_mut();
        unsafe {
            let callbacks = ztcore::ZT_Node_Callbacks {
                statePutFunction: transmute(zt_state_put_function::<T, N> as *const ()),
                stateGetFunction: transmute(zt_state_get_function::<T, N> as *const ()),
                wirePacketSendFunction: transmute(zt_wire_packet_send_function::<T, N> as *const ()),
                virtualNetworkFrameFunction: transmute(zt_virtual_network_frame_function::<T, N> as *const ()),
                virtualNetworkConfigFunction: transmute(zt_virtual_network_config_function::<T, N> as *const ()),
                eventCallback: transmute(zt_event_callback::<T, N> as *const ()),
                pathCheckFunction: transmute(zt_path_check_function::<T, N> as *const ()),
                pathLookupFunction: transmute(zt_path_lookup_function::<T, N> as *const ()),
            };
            let rc = ztcore::ZT_Node_new(&mut capi as *mut *mut ztcore::ZT_Node, transmute(&n as *const Node<T, N>), null_mut(), &callbacks as *const ztcore::ZT_Node_Callbacks, now);
            if rc != 0 {
                return Err(ResultCode::from_i32(rc as i32).unwrap_or(ResultCode::FatalErrorInternal));
            } else if capi.is_null() {
                return Err(ResultCode::FatalErrorInternal);
            }
        }
        n.capi = capi;

        Ok(n)
    }

    /// Perform periodic background tasks.
    /// The first call should happen no more than NODE_BACKGROUND_TASKS_MAX_INTERVAL milliseconds
    /// since the node was created, and after this runs it returns the amount of time the caller
    /// should wait before calling it again.
    pub fn process_background_tasks(&self) -> u64 {
        let current_time = now();
        self.now.set(current_time);

        let mut next_task_deadline: i64 = current_time;
        unsafe {
            ztcore::ZT_Node_processBackgroundTasks(self.capi, null_mut(), current_time, (&mut next_task_deadline as *mut i64).cast());
        }
        let mut next_delay = next_task_deadline - current_time;

        if next_delay < 1 {
            next_delay = 1;
        } else if next_delay > NODE_BACKGROUND_TASKS_MAX_INTERVAL as i64 {
            next_delay = NODE_BACKGROUND_TASKS_MAX_INTERVAL as i64;
        }
        next_delay as u64
    }

    fn delete_network_uptr(&self, nwid: u64) {
        let nptr = self.networks_by_id.lock().unwrap().remove(&nwid).unwrap_or(null_mut());
        if !nptr.is_null() {
            unsafe {
                Box::from_raw(nptr);
            }
        }
    }

    pub fn join(&self, nwid: NetworkId, controller_fingerprint: Option<Fingerprint>, network_obj: &Arc<N>) -> ResultCode {
        let mut cfp: MaybeUninit<ztcore::ZT_Fingerprint> = MaybeUninit::uninit();
        let mut cfpp: *mut ztcore::ZT_Fingerprint = null_mut();
        if controller_fingerprint.is_some() {
            let cfp2 = controller_fingerprint.unwrap();
            cfpp = cfp.as_mut_ptr();
            unsafe {
                (*cfpp).address = cfp2.address.0;
                (*cfpp).hash = cfp2.hash;
            }
        }

        let nptr = Box::into_raw(Box::new(network_obj.clone()));
        self.networks_by_id.lock().as_deref_mut().unwrap().insert(nwid.0, nptr);
        let rc = unsafe { ztcore::ZT_Node_join(self.capi, nwid.0, cfpp, nptr.cast::<c_void>(), null_mut()) };
        if rc != ztcore::ZT_ResultCode_ZT_RESULT_OK {
            self.delete_network_uptr(nwid.0);
        }
        return ResultCode::from_i32(rc as i32).unwrap_or(ResultCode::ErrorInternalNonFatal);
    }

    pub fn leave(&self, nwid: NetworkId) -> ResultCode {
        self.delete_network_uptr(nwid.0);
        unsafe {
            return ResultCode::from_i32(ztcore::ZT_Node_leave(self.capi, nwid.0, null_mut(), null_mut()) as i32).unwrap_or(ResultCode::ErrorInternalNonFatal);
        }
    }

    #[inline(always)]
    pub fn address(&self) -> Address {
        unsafe {
            return Address(ztcore::ZT_Node_address(self.capi) as u64);
        }
    }

    #[inline(always)]
    pub fn process_wire_packet(&self, local_socket: i64, remote_address: &InetAddress, data: Buffer) -> ResultCode {
        let current_time = self.now.get();
        let mut next_task_deadline: i64 = current_time;
        let rc = unsafe { ResultCode::from_i32(ztcore::ZT_Node_processWirePacket(self.capi, null_mut(), current_time, local_socket, remote_address.as_capi_ptr(), data.zt_core_buf as *const c_void, data.data_size as u32, 1, &mut next_task_deadline as *mut i64) as i32).unwrap_or(ResultCode::ErrorInternalNonFatal) };
        std::mem::forget(data); // prevent Buffer from being returned to ZT core twice, see comment in drop() in buffer.rs
        rc
    }

    #[inline(always)]
    pub fn process_virtual_network_frame(&self, nwid: &NetworkId, source_mac: &MAC, dest_mac: &MAC, ethertype: u16, vlan_id: u16, data: Buffer) -> ResultCode {
        let current_time = self.now.get();
        let mut next_tick_deadline: i64 = current_time;
        let rc = unsafe { ResultCode::from_i32(ztcore::ZT_Node_processVirtualNetworkFrame(self.capi, null_mut(), current_time, nwid.0, source_mac.0, dest_mac.0, ethertype as c_uint, vlan_id as c_uint, data.zt_core_buf as *const c_void, data.data_size as u32, 1, &mut next_tick_deadline as *mut i64) as i32).unwrap_or(ResultCode::ErrorInternalNonFatal) };
        std::mem::forget(data); // prevent Buffer from being returned to ZT core twice, see comment in drop() in buffer.rs
        rc
    }

    #[inline(always)]
    pub fn multicast_subscribe(&self, nwid: &NetworkId, multicast_group: &MAC, multicast_adi: u32) -> ResultCode {
        unsafe {
            return ResultCode::from_i32(ztcore::ZT_Node_multicastSubscribe(self.capi, null_mut(), nwid.0, multicast_group.0, multicast_adi as c_ulong) as i32).unwrap_or(ResultCode::ErrorInternalNonFatal);
        }
    }

    #[inline(always)]
    pub fn multicast_unsubscribe(&self, nwid: &NetworkId, multicast_group: &MAC, multicast_adi: u32) -> ResultCode {
        unsafe {
            return ResultCode::from_i32(ztcore::ZT_Node_multicastUnsubscribe(self.capi, nwid.0, multicast_group.0, multicast_adi as c_ulong) as i32).unwrap_or(ResultCode::ErrorInternalNonFatal);
        }
    }

    /// Get a copy of this node's identity.
    #[inline(always)]
    pub fn identity(&self) -> Identity {
        unsafe { self.identity_fast().clone() }
    }

    /// Get an identity that simply holds a pointer to the underlying node's identity.
    /// This is unsafe because the identity object becomes invalid if the node ceases
    /// to exist the Identity becomes invalid. Use clone() on it to get a copy.
    #[inline(always)]
    pub(crate) unsafe fn identity_fast(&self) -> Identity {
        Identity::new_from_capi(ztcore::ZT_Node_identity(self.capi), false)
    }

    pub fn status(&self) -> NodeStatus {
        let mut ns: MaybeUninit<ztcore::ZT_NodeStatus> = MaybeUninit::zeroed();
        unsafe {
            ztcore::ZT_Node_status(self.capi, ns.as_mut_ptr());
            let ns = ns.assume_init();
            if ns.identity.is_null() {
                panic!("ZT_Node_status() returned null identity");
            }
            return NodeStatus {
                address: Address(ns.address),
                identity: Identity::new_from_capi(&*ns.identity, false).clone(),
                public_identity: cstr_to_string(ns.publicIdentity, -1),
                secret_identity: cstr_to_string(ns.secretIdentity, -1),
                online: ns.online != 0,
            };
        }
    }

    pub fn peers(&self) -> Vec<Peer> {
        let mut p: Vec<Peer> = Vec::new();
        unsafe {
            let pl = ztcore::ZT_Node_peers(self.capi);
            if !pl.is_null() {
                let peer_count = (*pl).peerCount as usize;
                p.reserve(peer_count);
                for i in 0..peer_count as isize {
                    p.push(Peer::new_from_capi(&*(*pl).peers.offset(i)));
                }
                ztcore::ZT_freeQueryResult(pl as *const c_void);
            }
        }
        p
    }

    pub fn networks(&self) -> Vec<VirtualNetworkConfig> {
        let mut n: Vec<VirtualNetworkConfig> = Vec::new();
        unsafe {
            let nl = ztcore::ZT_Node_networks(self.capi);
            if !nl.is_null() {
                let net_count = (*nl).networkCount as usize;
                n.reserve(net_count);
                for i in 0..net_count as isize {
                    n.push(VirtualNetworkConfig::new_from_capi(&*(*nl).networks.offset(i)));
                }
                ztcore::ZT_freeQueryResult(nl as *const c_void);
            }
        }
        n
    }

    pub fn certificates(&self) -> Vec<(Certificate, u32)> {
        let mut c: Vec<(Certificate, u32)> = Vec::new();
        unsafe {
            let cl = ztcore::ZT_Node_listCertificates(self.capi);
            if !cl.is_null() {
                let cert_count = (*cl).certCount as usize;
                c.reserve(cert_count);
                for i in 0..cert_count as isize {
                    c.push((Certificate::new_from_capi(&**(*cl).certs.offset(i)), *(*cl).localTrust.offset(i)));
                }
                ztcore::ZT_freeQueryResult(cl as *const c_void);
            }
        }
        c
    }
}

unsafe impl<T: NodeEventHandler<N> + Sync + Send + Clone + 'static, N: 'static> Sync for Node<T, N> {}

unsafe impl<T: NodeEventHandler<N> + Sync + Send + Clone + 'static, N: 'static> Send for Node<T, N> {}

impl<T: NodeEventHandler<N> + Sync + Send + Clone + 'static,N: 'static> Drop for Node<T, N> {
    fn drop(&mut self) {
        // Manually take care of the unboxed Boxes in networks_by_id
        let mut nwids: Vec<u64> = Vec::new();
        for n in self.networks_by_id.lock().unwrap().iter() {
            nwids.push(*n.0);
        }
        for nwid in nwids.iter() {
            self.delete_network_uptr(*nwid);
        }

        unsafe {
            ztcore::ZT_Node_delete(self.capi, null_mut());
        }
    }
}
