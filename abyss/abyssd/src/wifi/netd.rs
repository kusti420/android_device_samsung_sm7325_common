//! A hand-written client for netd's INetd binder service ("netd", android.net.INetd).
//!
//! netd_aidl_interface (packages/modules/Connectivity/staticlibs/netd) has no Rust backend, so
//! abyssd declares the interface itself and writes the parcels by hand. Only methods with plain
//! int/String arguments plus one small parcelable are used. Transaction codes are
//! FIRST_CALL_TRANSACTION + the method's index in the frozen AIDL (stable AIDL keeps the order;
//! checked against the generated BnNetd.h of netd_aidl_interface-V18-cpp-source in this tree):
//!
//! | method | index |
//! |---|---|
//! | isAlive() | 0 |
//! | networkCreatePhysical(int netId, int permission) | 3 |
//! | networkDestroy(int netId) | 5 |
//! | networkAddInterface(int netId, String iface) | 6 |
//! | interfaceAddAddress(String ifName, String addr, int prefixLength) | 14 |
//! | networkAddRouteParcel(int netId, in RouteInfoParcel route) | 96 |
//! | networkSetDefault(int netId) | 66 |
//! | networkClearDefault() | 67 |
//! | interfaceSetCfg(InterfaceConfigurationParcel cfg) | 81 |
//! | interfaceClearAddrs(String ifName) | 83 |
//!
//! Strings are `@utf8InCpp String`, i.e. String16 on the wire (AParcel_writeString). Replies
//! start with the binder status header.
//!
//! Caller uid matters: netd's permission check (BinderUtil.h checkAnyPermission) passes uid
//! 1000 (system) at once but asks system_server's "permission" service for any other uid,
//! root included, and waits for it forever when system_server is stopped (IServiceManager.cpp
//! checkPermission loops with sleep(1)). In Abyss system_server is stopped, so this module is
//! only used from the abyss-net helper (uid system, netcfg.rs), never from abyssd (uid root).

use anyhow::{bail, Result};
use binder::binder_impl::{
    Binder, BorrowedParcel, IBinderInternal, Parcel, TransactionCode, FIRST_CALL_TRANSACTION,
    NON_NULL_PARCELABLE_FLAG,
};
use binder::{declare_binder_interface, Interface, Status, StatusCode, Strong};

/// Marker trait for the netd proxy (no generated methods; see `Netd` below).
pub trait INetd: Interface {}

declare_binder_interface! {
    INetd["android.net.INetd"] {
        native: BnNetd(on_transact),
        proxy: BpNetd,
    }
}

/// abyssd never serves INetd.
fn on_transact(
    _service: &dyn INetd,
    _code: TransactionCode,
    _data: &BorrowedParcel<'_>,
    _reply: &mut BorrowedParcel<'_>,
) -> std::result::Result<(), StatusCode> {
    Err(StatusCode::UNKNOWN_TRANSACTION)
}

impl INetd for BpNetd {}
impl INetd for Binder<BnNetd> {}

const IS_ALIVE: u32 = 0;
const NETWORK_CREATE_PHYSICAL: u32 = 3;
const NETWORK_DESTROY: u32 = 5;
const NETWORK_ADD_INTERFACE: u32 = 6;
const INTERFACE_ADD_ADDRESS: u32 = 14;
const NETWORK_ADD_ROUTE_PARCEL: u32 = 96;
const NETWORK_SET_DEFAULT: u32 = 66;
const NETWORK_CLEAR_DEFAULT: u32 = 67;
const INTERFACE_SET_CFG: u32 = 81;
const INTERFACE_CLEAR_ADDRS: u32 = 83;

/// The only uid netd answers without asking system_server (BinderUtil.h checkAnyPermission).
const AID_SYSTEM: u32 = 1000;

/// INetd.PERMISSION_NONE: any app may use the network (scripts later run as an app uid).
pub const PERMISSION_NONE: i32 = 0;

/// errno values netd returns as service-specific errors.
pub const ENONET: i32 = 64;
pub const EEXIST: i32 = 17;

/// A failed call: the method name plus netd's status.
#[derive(Debug)]
pub struct CallError {
    pub method: &'static str,
    /// Service-specific error (an errno for netd), 0 if the failure was something else.
    pub errno: i32,
    pub text: String,
}

impl std::fmt::Display for CallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.method, self.text)
    }
}

impl std::error::Error for CallError {}

pub struct Netd {
    b: Strong<dyn INetd>,
}

impl Netd {
    /// Looks the service up (servicemanager waits a few seconds if netd is restarting).
    pub fn connect() -> Result<Netd> {
        // Hard guard: a non-system caller would hang a netd binder thread forever (see above).
        let uid = nix::unistd::getuid().as_raw();
        if uid != AID_SYSTEM {
            bail!("refusing to call netd as uid {uid}: only uid {AID_SYSTEM} (abyss-net) may");
        }
        let b: Strong<dyn INetd> = match binder::get_interface("netd") {
            Ok(b) => b,
            Err(e) => bail!("netd service: {e:?}"),
        };
        Ok(Netd { b })
    }

    fn call<F>(&self, method: &'static str, index: u32, args: F) -> std::result::Result<Parcel, CallError>
    where
        F: FnOnce(&mut BorrowedParcel<'_>) -> std::result::Result<(), StatusCode>,
    {
        let fail = |text: String| CallError { method, errno: 0, text };
        let code: TransactionCode = FIRST_CALL_TRANSACTION + index;
        let reply = self
            .b
            .as_binder()
            .transact(code, 0, |mut p| args(&mut p))
            .map_err(|e| fail(format!("transaction failed: {e:?}")))?;
        let status: Status = reply.read().map_err(|e| fail(format!("bad reply: {e:?}")))?;
        if !status.is_ok() {
            return Err(CallError {
                method,
                errno: status.service_specific_error(),
                text: status.get_description(),
            });
        }
        Ok(reply)
    }

    fn call_void<F>(&self, method: &'static str, index: u32, args: F) -> std::result::Result<(), CallError>
    where
        F: FnOnce(&mut BorrowedParcel<'_>) -> std::result::Result<(), StatusCode>,
    {
        self.call(method, index, args).map(|_| ())
    }

    pub fn is_alive(&self) -> std::result::Result<bool, CallError> {
        let r = self.call("isAlive", IS_ALIVE, |_| Ok(()))?;
        r.read::<bool>().map_err(|e| CallError { method: "isAlive", errno: 0, text: format!("{e:?}") })
    }

    pub fn network_create_physical(&self, net_id: i32, permission: i32) -> std::result::Result<(), CallError> {
        self.call_void("networkCreatePhysical", NETWORK_CREATE_PHYSICAL, |p| {
            p.write(&net_id)?;
            p.write(&permission)
        })
    }

    pub fn network_destroy(&self, net_id: i32) -> std::result::Result<(), CallError> {
        self.call_void("networkDestroy", NETWORK_DESTROY, |p| p.write(&net_id))
    }

    pub fn network_add_interface(&self, net_id: i32, iface: &str) -> std::result::Result<(), CallError> {
        self.call_void("networkAddInterface", NETWORK_ADD_INTERFACE, |p| {
            p.write(&net_id)?;
            p.write(iface)
        })
    }

    pub fn interface_add_address(&self, ifname: &str, addr: &str, prefix: i32) -> std::result::Result<(), CallError> {
        self.call_void("interfaceAddAddress", INTERFACE_ADD_ADDRESS, |p| {
            p.write(ifname)?;
            p.write(addr)?;
            p.write(&prefix)
        })
    }

    /// `next_hop` "" = directly connected.
    pub fn network_add_route(
        &self,
        net_id: i32,
        ifname: &str,
        dest: &str,
        next_hop: &str,
    ) -> std::result::Result<(), CallError> {
        // networkAddRoute (index 61) is DEPRECATED in netd (NetdNativeService.cpp:967 returns
        // EX_UNSUPPORTED_OPERATION); ConnectivityService uses networkAddRouteParcel.
        // `in RouteInfoParcel route`: non-null marker, then the size-prefixed fields in AIDL
        // order (destination, ifName, nextHop, mtu, isLocalRoute).
        self.call_void("networkAddRouteParcel", NETWORK_ADD_ROUTE_PARCEL, |p| {
            p.write(&net_id)?;
            p.write(&NON_NULL_PARCELABLE_FLAG)?;
            p.sized_write(|sub| {
                sub.write(dest)?;
                sub.write(ifname)?;
                sub.write(next_hop)?;
                sub.write(&0i32)?;
                sub.write(&false)
            })
        })
    }

    pub fn network_set_default(&self, net_id: i32) -> std::result::Result<(), CallError> {
        self.call_void("networkSetDefault", NETWORK_SET_DEFAULT, |p| p.write(&net_id))
    }

    pub fn network_clear_default(&self) -> std::result::Result<(), CallError> {
        self.call_void("networkClearDefault", NETWORK_CLEAR_DEFAULT, |_| Ok(()))
    }

    pub fn interface_clear_addrs(&self, ifname: &str) -> std::result::Result<(), CallError> {
        self.call_void("interfaceClearAddrs", INTERFACE_CLEAR_ADDRS, |p| p.write(ifname))
    }

    /// interfaceSetCfg with only a flag ("up" / "down"). netd clears the IPv4 address first and
    /// then fails "Failed to add addr" for the empty address after applying the flag; that error
    /// is expected and returned like any other (callers ignore it for "down").
    pub fn interface_set_flag(&self, ifname: &str, flag: &str) -> std::result::Result<(), CallError> {
        let flags = vec![flag.to_string()];
        self.call_void("interfaceSetCfg", INTERFACE_SET_CFG, |p| {
            // `in InterfaceConfigurationParcel cfg`: non-null marker, then the size-prefixed
            // parcelable (ifName, hwAddr, ipv4Addr, prefixLength, flags), as the generated
            // code writes it.
            p.write(&NON_NULL_PARCELABLE_FLAG)?;
            p.sized_write(|sub| {
                sub.write(ifname)?;
                sub.write("")?;
                sub.write("")?;
                sub.write(&0i32)?;
                sub.write(&flags)
            })
        })
    }
}
