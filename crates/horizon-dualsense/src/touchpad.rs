//! A DualShock 4's or DualSense's touchpad, read from the pad's own input reports
//! (docs/controllers.md, "Touchpad"). Gilrs has no touchpad, so the pad's HID collection is opened
//! a second time for reading, beside the write handle in [`crate::xbox`], and a thread publishes
//! the two contacts it carries.
//!
//! The byte layouts are SDL's (`SDL_hidapi_ps4.c`, `SDL_hidapi_ps5.c`): a DualShock 4's state
//! packet starts at byte 1 of report 0x01 (USB) or byte 3 of reports 0x11..0x19 (Bluetooth, with
//! two extra bytes); a DualSense's at byte 1 of 0x01 or byte 2 of 0x31. Each contact is a counter
//! byte, whose top bit says the finger is lifted, and three data bytes: 12 bits of x and y. The
//! pad is 1920 steps wide; 920 tall on a DualShock 4 (the hardware says 944, but SDL's 920 feels
//! better) and 1070 on a DualSense.
use crate::dualshock;
use crate::xbox::Target;

/// A finger on the touchpad, its position across the pad's own surface, 0..1.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Finger {
    /// Left to right.
    pub x: f32,
    /// Top to bottom.
    pub y: f32,
}

/// The two contacts a report carries, `None` where that finger is lifted.
pub type Contacts = [Option<Finger>; 2];

/// The two Sony pads with a touchpad, whose reports place it differently.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pad {
    DualShock,
    DualSense,
}

impl Pad {
    /// The pad with these ids, if it has a touchpad.
    pub fn of(vendor: u16, product: u16) -> Option<Pad> {
        if dualshock::is_dualshock(vendor, product) {
            Some(Pad::DualShock)
        } else if crate::is_dualsense(vendor, product) {
            Some(Pad::DualSense)
        } else {
            None
        }
    }

    /// The contacts in an input report, or `None` for a report this pad doesn't carry them in
    /// (an output report, a short one, another pad's).
    pub fn contacts(self, report: &[u8]) -> Option<Contacts> {
        let state = match (self, *report.first()?) {
            (_, 0x01) => report.get(1..)?,
            (Pad::DualShock, 0x11..=0x19) => report.get(3..)?,
            (Pad::DualSense, 0x31) => report.get(2..)?,
            _ => return None,
        };
        // The first finger's counter, its three data bytes, then the second finger's four.
        let at = match self {
            Pad::DualShock => 34,
            Pad::DualSense => 32,
        };
        let block = state.get(at..at + 8)?;
        let (width, height) = match self {
            Pad::DualShock => (1920.0, 920.0),
            Pad::DualSense => (1920.0, 1070.0),
        };
        Some([
            finger(block[0], &block[1..4], width, height),
            finger(block[4], &block[5..8], width, height),
        ])
    }
}

/// One contact: its counter byte (bit 7 set while lifted) and its three data bytes.
fn finger(counter: u8, data: &[u8], width: f32, height: f32) -> Option<Finger> {
    if counter & 0x80 != 0 {
        return None;
    }
    // x is data[0] and the low nibble of data[1]; y is the high nibble of data[1] and data[2].
    let x = u16::from(data[0]) | (u16::from(data[1] & 0x0f) << 8);
    let y = u16::from(data[1] >> 4) | (u16::from(data[2]) << 4);
    Some(Finger {
        x: (f32::from(x) / width).clamp(0.0, 1.0),
        y: (f32::from(y) / height).clamp(0.0, 1.0),
    })
}

#[cfg(not(target_os = "ios"))]
pub use reader::Touchpad;

/// The reader: hidapi has no backend on iOS, so there the pad is never opened.
#[cfg(not(target_os = "ios"))]
mod reader {
    use super::*;
    use hidapi::{BusType, HidApi, HidDevice};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread::JoinHandle;
    use std::time::{Duration, Instant};

    #[derive(Default)]
    struct Shared {
        contacts: Contacts,
        /// How many reports changed the contacts, so a caller can tell new input from a still
        /// finger.
        generation: u64,
    }

    /// A pad's touchpad, read on its own thread until this is dropped. It opens the pad again on
    /// its own when it comes back.
    pub struct Touchpad {
        shared: Arc<Mutex<Shared>>,
        stop: Arc<AtomicBool>,
        thread: Option<JoinHandle<()>>,
    }

    impl Touchpad {
        /// Start reading the pad `target` names (the pad the output writes to).
        pub fn start(target: Target) -> Touchpad {
            let shared = Arc::new(Mutex::new(Shared::default()));
            let stop = Arc::new(AtomicBool::new(false));
            let thread = {
                let (shared, stop) = (shared.clone(), stop.clone());
                std::thread::Builder::new()
                    .name("touchpad".into())
                    .spawn(move || Reader::new(target, shared, stop).run())
                    .map_err(|e| log::warn!("Horizon: touchpad reader: {e}"))
                    .ok()
            };
            Touchpad {
                shared,
                stop,
                thread,
            }
        }

        /// The contacts of the latest report, and how many reports changed them.
        pub fn latest(&self) -> (Contacts, u64) {
            let shared = self.shared.lock().expect("touchpad state");
            (shared.contacts, shared.generation)
        }
    }

    impl Drop for Touchpad {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    struct Reader {
        target: Target,
        api: Option<HidApi>,
        device: Option<(HidDevice, Pad)>,
        shared: Arc<Mutex<Shared>>,
        stop: Arc<AtomicBool>,
        /// When to look for the pad again, after it wasn't there or read failed.
        next_open: Instant,
    }

    impl Reader {
        fn new(target: Target, shared: Arc<Mutex<Shared>>, stop: Arc<AtomicBool>) -> Self {
            Reader {
                target,
                api: None,
                device: None,
                shared,
                stop,
                next_open: Instant::now(),
            }
        }

        fn run(mut self) {
            let mut buffer = [0u8; 256];
            while !self.stop.load(Ordering::Relaxed) {
                if self.device.is_none() {
                    if Instant::now() < self.next_open {
                        std::thread::sleep(Duration::from_millis(50));
                        continue;
                    }
                    self.device = self.open();
                    if self.device.is_none() {
                        self.next_open = Instant::now() + Duration::from_secs(1);
                        continue;
                    }
                }
                let (device, pad) = self.device.as_ref().expect("a pad is open");
                match device.read_timeout(&mut buffer, 50) {
                    // A timeout: nothing new, or a report in a layout this pad doesn't send.
                    Ok(0) => {}
                    Ok(len) => {
                        let Some(contacts) = pad.contacts(&buffer[..len]) else {
                            continue;
                        };
                        if let Ok(mut shared) = self.shared.lock()
                            && shared.contacts != contacts
                        {
                            shared.contacts = contacts;
                            shared.generation = shared.generation.wrapping_add(1);
                        }
                    }
                    Err(_) => {
                        // Unplugged, or the handle went stale: open the pad again in a second.
                        self.device = None;
                        self.next_open = Instant::now() + Duration::from_secs(1);
                    }
                }
            }
        }

        fn open(&mut self) -> Option<(HidDevice, Pad)> {
            let api = match self.api.as_mut() {
                Some(api) => api.refresh_devices().map(|()| &*api),
                None => HidApi::new().map(|api| &*self.api.insert(api)),
            };
            let api = match api {
                Ok(api) => api,
                Err(e) => {
                    log::debug!("Horizon: touchpad: no HID API: {e}");
                    return None;
                }
            };
            // The same order the output opens pads in: a DualShock 4 lists a gamepad collection
            // and a vendor-specific one, and the gamepad one is the pad's own reports. Wired
            // first, as a pad on a cable can still be listed over Bluetooth for a moment.
            let mut pads: Vec<&hidapi::DeviceInfo> = api
                .device_list()
                .filter(|d| {
                    let (vendor, product) = (d.vendor_id(), d.product_id());
                    Pad::of(vendor, product).is_some() && self.target.matches(vendor, product)
                })
                .collect();
            pads.sort_by_key(|d| {
                let gamepad = d.usage_page() == 1 && d.usage() == 5;
                (!gamepad, matches!(d.bus_type(), BusType::Bluetooth))
            });
            let rank = if self.target == Target::ANY {
                0
            } else {
                self.target.index
            };
            let info = pads.get(rank).or_else(|| pads.first()).copied()?;
            let pad = Pad::of(info.vendor_id(), info.product_id())?;
            match info.open_device(api) {
                Ok(device) => Some((device, pad)),
                Err(e) => {
                    log::debug!("Horizon: touchpad: {e}");
                    None
                }
            }
        }
    }
}

/// No reader on iOS: hidapi has no backend there.
#[cfg(target_os = "ios")]
pub struct Touchpad {
    _private: (),
}

#[cfg(target_os = "ios")]
impl Touchpad {
    pub fn start(_target: Target) -> Touchpad {
        Touchpad { _private: () }
    }

    pub fn latest(&self) -> (Contacts, u64) {
        ([None; 2], 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A report of `len` zero bytes with `id` in front, and a contact written at `counter`.
    fn report(id: u8, len: usize, counter: usize) -> Vec<u8> {
        let mut report = vec![0u8; len];
        report[0] = id;
        report[counter] = 0x80; // lifted
        report
    }

    /// Put a finger at `(x, y)` in the contact whose counter is at `counter`.
    fn touch(report: &mut [u8], counter: usize, x: u16, y: u16) {
        report[counter] = 0x00;
        // data[0] is x's low byte, data[1] x's high nibble and y's low nibble, data[2] y's
        // remaining 8 bits.
        report[counter + 1] = x as u8;
        report[counter + 2] = (((x >> 8) as u8) & 0x0f) | (((y & 0x0f) as u8) << 4);
        report[counter + 3] = (y >> 4) as u8;
    }

    fn near(finger: Option<Finger>, x: f32, y: f32) {
        let finger = finger.expect("a finger");
        assert!(
            (finger.x - x).abs() < 0.001 && (finger.y - y).abs() < 0.001,
            "{finger:?} is not ({x}, {y})"
        );
    }

    #[test]
    fn a_dualshock_4s_two_contacts_over_usb() {
        // Report 0x01, state at byte 1, the touch block at 34: counters at 35 and 39.
        let mut report = report(0x01, 1 + 42, 35);
        report[39] = 0x80;
        touch(&mut report, 35, 960, 460);
        let contacts = Pad::DualShock.contacts(&report).expect("a state report");
        near(contacts[0], 0.5, 0.5);
        assert_eq!(contacts[1], None, "the second finger is lifted");
    }

    #[test]
    fn a_dualshock_4s_bluetooth_report_has_two_bytes_in_front() {
        // Reports 0x11..0x19 carry the state at byte 3; the counters then sit at 37 and 41.
        let mut report = report(0x11, 3 + 42, 37);
        report[41] = 0x80;
        touch(&mut report, 37, 1920, 920);
        let contacts = Pad::DualShock.contacts(&report).expect("a state report");
        near(contacts[0], 1.0, 1.0);
        assert_eq!(contacts[1], None);
    }

    #[test]
    fn a_dualsenses_two_contacts_over_usb() {
        // Report 0x01, state at byte 1, the touch block at 32: counters at 33 and 37.
        let mut report = report(0x01, 1 + 40, 33);
        touch(&mut report, 33, 0, 0);
        touch(&mut report, 37, 1920, 1070);
        let contacts = Pad::DualSense.contacts(&report).expect("a state report");
        near(contacts[0], 0.0, 0.0);
        near(contacts[1], 1.0, 1.0);
    }

    #[test]
    fn a_dualsenses_bluetooth_report_starts_at_byte_two() {
        // Report 0x31, state at byte 2; the counters then sit at 34 and 38.
        let mut report = report(0x31, 2 + 40, 34);
        report[38] = 0x80;
        touch(&mut report, 34, 960, 535);
        let contacts = Pad::DualSense.contacts(&report).expect("a state report");
        near(contacts[0], 0.5, 0.5);
        assert_eq!(contacts[1], None);
    }

    #[test]
    fn only_state_reports_carry_contacts() {
        // An effects report, a short state report, and another pad's are all `None`.
        assert_eq!(
            Pad::DualShock.contacts(&[0x05; 43]),
            None,
            "an effects report"
        );
        assert_eq!(Pad::DualShock.contacts(&[0x01; 10]), None, "too short");
        assert_eq!(
            Pad::DualSense.contacts(&[0x02; 64]),
            None,
            "an effects report"
        );
        assert_eq!(Pad::DualSense.contacts(&[]), None, "empty");
    }

    #[test]
    fn the_pads_with_a_touchpad() {
        assert_eq!(Pad::of(0x054c, 0x05c4), Some(Pad::DualShock));
        assert_eq!(Pad::of(0x054c, 0x09cc), Some(Pad::DualShock));
        assert_eq!(Pad::of(0x054c, 0x0ce6), Some(Pad::DualSense));
        assert_eq!(Pad::of(0x054c, 0x0df2), Some(Pad::DualSense));
        // An Xbox pad, and a Sony device that isn't a pad.
        assert_eq!(Pad::of(0x045e, 0x02fd), None);
        assert_eq!(Pad::of(0x054c, 0x0ba0), None);
    }
}
