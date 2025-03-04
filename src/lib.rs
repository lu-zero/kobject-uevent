//! [netlink](https://www.man7.org/linux/man-pages/man7/netlink.7.html) `NETLINK_KOBJECT_UEVENT` packet parser
//!
//! The [uevents](https://www.kernel.org/doc/html/latest/core-api/kobject.html#uevents) are
//! triggered by `kobject_uevent` and `kobject_uevent_env` to signal a change in the referred kobject.

use std::{
    collections::HashMap,
    io,
    path::{Path, PathBuf},
    str::{from_utf8, FromStr},
};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Unexpected action: {0}")]
    UnexpectedAction(String),
    #[error("Invalid DEVPATH: {0}")]
    InvalidDevPath(String),
    #[error("Unexpected SEQNUM: {0}")]
    InvalidSeqNum(String),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("Path not inside mountpoint")]
    NotInsideMountpoint,
    #[error("Packet not UTF-8")]
    NotUtf8,
    #[error("action not found")]
    ActionNotFound,
    #[error("devpath not found")]
    DevPathNotFound,
    #[error("subsystem not found")]
    SubsystemNotFound,
    #[error("seq missing")]
    SeqMissing,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash)]
/// KObject action types
///
/// See kobject_action in include/linux/kobject.h
pub enum ActionType {
    /// A new kobject is added
    Add,
    /// A kobject is removed
    Remove,
    /// the kobject changed its internal state
    ///
    /// the `env` contains kobject-specific information.
    Change,
    /// the kobject is reparented as a result of `kobject_move`
    ///
    /// the `env` contains `DEVPATH_OLD=<oldpath>`.
    Move,
    /// The device is back online after successful `device_offline`.
    Online,
    /// The device is ready to be hot-removed.
    Offline,
    /// The device is bound to a driver.
    Bind,
    /// The device is not bound to its driver anymore.
    Unbind,
}

impl FromStr for ActionType {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        use ActionType::*;
        match s {
            "add" => Ok(Add),
            "remove" => Ok(Remove),
            "change" => Ok(Change),
            "move" => Ok(Move),
            "online" => Ok(Online),
            "offline" => Ok(Offline),
            "bind" => Ok(Bind),
            "unbind" => Ok(Unbind),
            _ => Err(Error::UnexpectedAction(s.to_owned())),
        }
    }
}

/// Linux kernel userspace event
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UEvent {
    /// Action happening
    pub action: ActionType,
    /// Complete Kernel Object path
    pub devpath: PathBuf,
    /// SubSystem originating the event
    pub subsystem: String,
    /// Arguments
    pub env: HashMap<String, String>,
    /// Sequence number
    pub seq: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MaybeUEvent {
    /// Action happening
    pub action: Option<ActionType>,
    /// Complete Kernel Object path
    pub devpath: Option<PathBuf>,
    /// SubSystem originating the event
    pub subsystem: Option<String>,
    /// Arguments
    pub env: HashMap<String, String>,
    /// Sequence number
    pub seq: Option<u64>,
}

/// Parse key=value strings as UEvent, some fields may be missing
fn parse_uevent_iter<'a>(iter: impl Iterator<Item = &'a str>) -> Result<MaybeUEvent, Error> {
    let mut action = None;
    let mut devpath = None;
    let mut subsystem = None;
    let mut env = HashMap::new();
    let mut seq = None;

    for f in iter {
        if let Some((key, value)) = f.split_once('=') {
            match key {
                "ACTION" => action = Some(value.parse::<ActionType>()?),
                "DEVPATH" => {
                    devpath = Some(
                        value
                            .parse::<PathBuf>()
                            .map_err(|_| Error::InvalidDevPath(value.to_owned()))?,
                    )
                }
                "SUBSYSTEM" => subsystem = Some(value.to_string()),
                "SEQNUM" => {
                    seq = Some(
                        value
                            .parse::<u64>()
                            .map_err(|_| Error::InvalidSeqNum(value.to_owned()))?,
                    )
                }
                _ => {}
            }
            let _ = env.insert(key.into(), value.into());
        }
    }

    Ok(MaybeUEvent {
        action,
        devpath,
        subsystem,
        env,
        seq,
    })
}

impl UEvent {
    /// Parse a sysfs path as an Add UEvent
    pub fn from_sysfs_path(
        path: impl AsRef<Path>,
        mountpoint: impl AsRef<Path>,
    ) -> Result<UEvent, Error> {
        let path = path.as_ref();
        let uevent = std::fs::read_to_string(path.join("uevent"))?;
        let subsystem_path = std::fs::read_link(path.join("subsystem"))?;
        let lines = uevent.lines();

        let MaybeUEvent { env, .. } = parse_uevent_iter(lines)?;

        let action = ActionType::Add;
        // make it look like a netlink devpath
        let devpath = Path::new("/").join(
            path.canonicalize()?
                .strip_prefix(mountpoint)
                .map_err(|_| Error::NotInsideMountpoint)?,
        );
        let subsystem = subsystem_path
            .file_name()
            .ok_or(Error::SubsystemNotFound)?
            .to_string_lossy()
            .to_string();
        let seq = 0;

        Ok(UEvent {
            action,
            devpath,
            subsystem,
            env,
            seq,
        })
    }

    /// Parse a netlink packet as received from the NETLINK_KOBJECT_UEVENT broadcast
    pub fn from_netlink_packet(pkt: &[u8]) -> Result<UEvent, Error> {
        let lines = from_utf8(pkt).map_err(|_| Error::NotUtf8)?.split('\0');
        let MaybeUEvent {
            action,
            devpath,
            subsystem,
            env,
            seq,
        } = parse_uevent_iter(lines)?;

        let action = action.ok_or(Error::ActionNotFound)?;
        let devpath = devpath.ok_or(Error::DevPathNotFound)?;
        let subsystem = subsystem.ok_or(Error::SubsystemNotFound)?;
        let seq = seq.ok_or(Error::SeqMissing)?;

        Ok(UEvent {
            action,
            devpath,
            subsystem,
            env,
            seq,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    macro_rules! uevent {
        (
            action: $action:expr,
             devpath: $devpath:expr,
             subsystem: $subsystem:expr,
             env: { $($env_name:expr => $env_value:expr),* $(,)? },
             seq: $seq:expr
         ) => {
            UEvent {
                action: $action,
                devpath: PathBuf::from($devpath),
                subsystem: $subsystem.to_string(),
                env: IntoIterator::into_iter([
                    $(($env_name.to_string(), $env_value.to_string())),*
                ]).collect(),
                seq: $seq,
            }
        };
    }

    #[test]
    fn add_uevent() {
        const DATA: &[u8] = b"add@/devices/platform/serial8250/tty/ttyS6\0\
                              ACTION=add\0\
                              DEVPATH=/devices/platform/serial8250/tty/ttyS6\0\
                              SUBSYSTEM=tty\0\
                              SYNTH_UUID=0\0\
                              MAJOR=4\0\
                              MINOR=70\0\
                              DEVNAME=ttyS6\0\
                              SEQNUM=3469";
        assert_eq!(
            UEvent::from_netlink_packet(DATA).unwrap(),
            uevent! {
                action: ActionType::Add,
                devpath: "/devices/platform/serial8250/tty/ttyS6",
                subsystem: "tty",
                env: {
                    "ACTION" => "add",
                    "DEVPATH" => "/devices/platform/serial8250/tty/ttyS6",
                    "SUBSYSTEM" => "tty",
                    "SYNTH_UUID" => "0",
                    "MAJOR" => "4",
                    "MINOR" => "70",
                    "DEVNAME" => "ttyS6",
                    "SEQNUM" => "3469",
                },
                seq: 3469
            }
        );
    }

    #[test]
    fn remove_uevent() {
        const DATA: &[u8] = b"remove@/devices/platform/serial8250/tty/ttyS6\0\
                              ACTION=remove\0\
                              DEVPATH=/devices/platform/serial8250/tty/ttyS6\0\
                              SUBSYSTEM=tty\0\
                              SYNTH_UUID=0\0\
                              MAJOR=4\0\
                              MINOR=70\0\
                              DEVNAME=ttyS6\0\
                              SEQNUM=3471";
        assert_eq!(
            UEvent::from_netlink_packet(DATA).unwrap(),
            uevent! {
                action: ActionType::Remove,
                devpath: "/devices/platform/serial8250/tty/ttyS6",
                subsystem: "tty",
                env: {
                    "ACTION" => "remove",
                    "DEVPATH" => "/devices/platform/serial8250/tty/ttyS6",
                    "SUBSYSTEM" => "tty",
                    "SYNTH_UUID" => "0",
                    "MAJOR" => "4",
                    "MINOR" => "70",
                    "DEVNAME" => "ttyS6",
                    "SEQNUM" => "3471",
                },
                seq: 3471
            }
        );
    }

    #[test]
    fn change_uevent() {
        const DATA: &[u8] = b"change@/devices/platform/serial8250/tty/ttyS6\0\
                              ACTION=change\0\
                              DEVPATH=/devices/platform/serial8250/tty/ttyS6\0\
                              SUBSYSTEM=tty\0\
                              SYNTH_UUID=0\0\
                              MAJOR=4\0\
                              MINOR=70\0\
                              DEVNAME=ttyS6\0\
                              SEQNUM=3472";
        assert_eq!(
            UEvent::from_netlink_packet(DATA).unwrap(),
            uevent! {
                action: ActionType::Change,
                devpath: "/devices/platform/serial8250/tty/ttyS6",
                subsystem: "tty",
                env: {
                    "ACTION" => "change",
                    "DEVPATH" => "/devices/platform/serial8250/tty/ttyS6",
                    "SUBSYSTEM" => "tty",
                    "SYNTH_UUID" => "0",
                    "MAJOR" => "4",
                    "MINOR" => "70",
                    "DEVNAME" => "ttyS6",
                    "SEQNUM" => "3472",
                },
                seq: 3472
            }
        );
    }

    #[test]
    fn move_uevent() {
        const DATA: &[u8] = b"move@/devices/platform/serial8250/tty/ttyS6\0\
                              ACTION=move\0\
                              DEVPATH=/devices/platform/serial8250/tty/ttyS6\0\
                              SUBSYSTEM=tty\0\
                              SYNTH_UUID=0\0\
                              MAJOR=4\0\
                              MINOR=70\0\
                              DEVNAME=ttyS6\0\
                              SEQNUM=3473";
        assert_eq!(
            UEvent::from_netlink_packet(DATA).unwrap(),
            uevent! {
                action: ActionType::Move,
                devpath: "/devices/platform/serial8250/tty/ttyS6",
                subsystem: "tty",
                env: {
                    "ACTION" => "move",
                    "DEVPATH" => "/devices/platform/serial8250/tty/ttyS6",
                    "SUBSYSTEM" => "tty",
                    "SYNTH_UUID" => "0",
                    "MAJOR" => "4",
                    "MINOR" => "70",
                    "DEVNAME" => "ttyS6",
                    "SEQNUM" => "3473",
                },
                seq: 3473
            }
        );
    }

    #[test]
    fn online_uevent() {
        const DATA: &[u8] = b"online@/devices/platform/serial8250/tty/ttyS6\0\
                              ACTION=online\0\
                              DEVPATH=/devices/platform/serial8250/tty/ttyS6\0\
                              SUBSYSTEM=tty\0\
                              SYNTH_UUID=0\0\
                              MAJOR=4\0\
                              MINOR=70\0\
                              DEVNAME=ttyS6\0\
                              SEQNUM=3474";
        assert_eq!(
            UEvent::from_netlink_packet(DATA).unwrap(),
            uevent! {
                action: ActionType::Online,
                devpath: "/devices/platform/serial8250/tty/ttyS6",
                subsystem: "tty",
                env: {
                    "ACTION" => "online",
                    "DEVPATH" => "/devices/platform/serial8250/tty/ttyS6",
                    "SUBSYSTEM" => "tty",
                    "SYNTH_UUID" => "0",
                    "MAJOR" => "4",
                    "MINOR" => "70",
                    "DEVNAME" => "ttyS6",
                    "SEQNUM" => "3474",
                },
                seq: 3474
            }
        );
    }

    #[test]
    fn offline_uevent() {
        const DATA: &[u8] = b"offline@/devices/platform/serial8250/tty/ttyS6\0\
                              ACTION=offline\0\
                              DEVPATH=/devices/platform/serial8250/tty/ttyS6\0\
                              SUBSYSTEM=tty\0\
                              SYNTH_UUID=0\0\
                              MAJOR=4\0\
                              MINOR=70\0\
                              DEVNAME=ttyS6\0\
                              SEQNUM=3475";
        assert_eq!(
            UEvent::from_netlink_packet(DATA).unwrap(),
            uevent! {
                action: ActionType::Offline,
                devpath: "/devices/platform/serial8250/tty/ttyS6",
                subsystem: "tty",
                env: {
                    "ACTION" => "offline",
                    "DEVPATH" => "/devices/platform/serial8250/tty/ttyS6",
                    "SUBSYSTEM" => "tty",
                    "SYNTH_UUID" => "0",
                    "MAJOR" => "4",
                    "MINOR" => "70",
                    "DEVNAME" => "ttyS6",
                    "SEQNUM" => "3475",
                },
                seq: 3475
            }
        );
    }

    #[test]
    fn bind_uevent() {
        const DATA: &[u8] = b"bind@/devices/platform/serial8250/tty/ttyS6\0\
                              ACTION=bind\0\
                              DEVPATH=/devices/platform/serial8250/tty/ttyS6\0\
                              SUBSYSTEM=tty\0\
                              SYNTH_UUID=0\0\
                              MAJOR=4\0\
                              MINOR=70\0\
                              DEVNAME=ttyS6\0\
                              SEQNUM=3476";
        assert_eq!(
            UEvent::from_netlink_packet(DATA).unwrap(),
            uevent! {
                action: ActionType::Bind,
                devpath: "/devices/platform/serial8250/tty/ttyS6",
                subsystem: "tty",
                env: {
                    "ACTION" => "bind",
                    "DEVPATH" => "/devices/platform/serial8250/tty/ttyS6",
                    "SUBSYSTEM" => "tty",
                    "SYNTH_UUID" => "0",
                    "MAJOR" => "4",
                    "MINOR" => "70",
                    "DEVNAME" => "ttyS6",
                    "SEQNUM" => "3476",
                },
                seq: 3476
            }
        );
    }

    #[test]
    fn unbind_uevent() {
        const DATA: &[u8] = b"unbind@/devices/platform/serial8250/tty/ttyS6\0\
                              ACTION=unbind\0\
                              DEVPATH=/devices/platform/serial8250/tty/ttyS6\0\
                              SUBSYSTEM=tty\0\
                              SYNTH_UUID=0\0\
                              MAJOR=4\0\
                              MINOR=70\0\
                              DEVNAME=ttyS6\0\
                              SEQNUM=3477";
        assert_eq!(
            UEvent::from_netlink_packet(DATA).unwrap(),
            uevent! {
                action: ActionType::Unbind,
                devpath: "/devices/platform/serial8250/tty/ttyS6",
                subsystem: "tty",
                env: {
                    "ACTION" => "unbind",
                    "DEVPATH" => "/devices/platform/serial8250/tty/ttyS6",
                    "SUBSYSTEM" => "tty",
                    "SYNTH_UUID" => "0",
                    "MAJOR" => "4",
                    "MINOR" => "70",
                    "DEVNAME" => "ttyS6",
                    "SEQNUM" => "3477",
                },
                seq: 3477
            }
        );
    }

    #[test]
    fn invalid_event() {
        const DATA: &[u8] = b"hello@/devices/platform/serial8250/tty/ttyS6\0\
                              ACTION=hello\0\
                              DEVPATH=/devices/platform/serial8250/tty/ttyS6\0\
                              SUBSYSTEM=tty\0\
                              SEQNUM=3477";
        assert!(UEvent::from_netlink_packet(DATA).is_err());
    }

    #[test]
    fn missing_action() {
        const DATA: &[u8] = b"add@/devices/platform/serial8250/tty/ttyS6\0\
                              DEVPATH=/devices/platform/serial8250/tty/ttyS6\0\
                              SUBSYSTEM=tty\0\
                              SEQNUM=3477";
        assert!(UEvent::from_netlink_packet(DATA).is_err());
    }

    #[test]
    fn missing_devpath() {
        const DATA: &[u8] = b"add@/devices/platform/serial8250/tty/ttyS6\0\
                              ACTION=unbind\0\
                              SUBSYSTEM=tty\0\
                              SEQNUM=3477";
        assert!(UEvent::from_netlink_packet(DATA).is_err());
    }

    #[test]
    fn missing_subsystem() {
        const DATA: &[u8] = b"add@/devices/platform/serial8250/tty/ttyS6\0\
                              ACTION=unbind\0\
                              DEVPATH=/devices/platform/serial8250/tty/ttyS6\0\
                              SEQNUM=3477";
        assert!(UEvent::from_netlink_packet(DATA).is_err());
    }

    #[test]
    fn missing_seqnum() {
        const DATA: &[u8] = b"add@/devices/platform/serial8250/tty/ttyS6\0\
                              ACTION=unbind\0\
                              DEVPATH=/devices/platform/serial8250/tty/ttyS6\0\
                              SUBSYSTEM=tty";
        assert!(UEvent::from_netlink_packet(DATA).is_err());
    }
}
