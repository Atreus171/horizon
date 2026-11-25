//! The pad over hidapi and Windows.Gaming.Input, and the thread that writes to it: writes block
//! (a Bluetooth report can take milliseconds), so the game only hands over what the pad should
//! do.
use crate::dualshock;
#[cfg(windows)]
use crate::xbox::wgi;
use crate::xbox::{Motors, Target};
#[cfg(not(target_os = "ios"))]
use crate::xbox::{bluetooth, is_xbox_one, magnitude, usb};
#[cfg(not(target_os = "ios"))]
use hidapi::{BusType, HidApi, HidDevice, HidError};
#[cfg(not(target_os = "ios"))]
use std::fmt;
use std::sync::Arc;
#[cfg(not(target_os = "ios"))]
use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::mpsc::Sender;
#[cfg(not(target_os = "ios"))]
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread::JoinHandle;
use std::time::Duration;
#[cfg(not(target_os = "ios"))]
use std::time::Instant;

/// Why a pad could not take a report.
#[cfg(not(target_os = "ios"))]
#[derive(Debug)]
pub enum Error {
    /// hidapi refused the report: a pad that went away, an endpoint another process holds.
    Hid(HidError),
    /// Windows.Gaming.Input refused it.
    #[cfg(windows)]
    Wgi(windows::core::Error),
}

#[cfg(not(target_os = "ios"))]
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Hid(e) => write!(f, "{e}"),
            #[cfg(windows)]
            Error::Wgi(e) => write!(f, "Windows.Gaming.Input: {e}"),
        }
    }
}

#[cfg(not(target_os = "ios"))]
impl std::error::Error for Error {}

#[cfg(not(target_os = "ios"))]
pub type Result<T> = std::result::Result<T, Error>;

#[cfg(not(target_os = "ios"))]
impl From<HidError> for Error {
    fn from(e: HidError) -> Self {
        Error::Hid(e)
    }
}

#[cfg(windows)]
impl From<windows::core::Error> for Error {
    fn from(e: windows::core::Error) -> Self {
        Error::Wgi(e)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transport {
    Usb,
    Bluetooth,
}

/// Which pad a [`Xbox`] drives, and so which reports it writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Frame {
    /// An Xbox One pad's GIP reports (SDL's `SDL_hidapi_xboxone.c`), or Windows.Gaming.Input.
    Gip,
    /// A DualShock 4's own reports ([`crate::dualshock`]).
    Dualshock,
}

/// Whether a pad on this transport takes a HID output report on this OS. On Windows a wired pad's
/// HID endpoint is XGIP's stand-in, which takes none (SDL_hidapi_xboxone.c skips every
/// `\\?\HID#` path there), so Windows.Gaming.Input is the way to its trigger motors ([`wgi`]);
/// one on Bluetooth is a real HID device, which Chromium's own Xbox HID writer drives on every OS.
pub fn output_supported(transport: Transport) -> bool {
    !(cfg!(target_os = "windows") && transport == Transport::Usb)
}

/// An open pad the output drives itself: its HID endpoint, or Windows.Gaming.Input on Windows.
#[cfg(not(target_os = "ios"))]
pub struct Xbox {
    backend: Backend,
    transport: Transport,
    /// Which reports it takes: an Xbox One pad's or a DualShock 4's.
    frame: Frame,
    /// Which of the DualShock 4's reports a Bluetooth link takes, and in which layout.
    report: Report,
}

/// The report a Bluetooth DualShock 4 is given. Sony's pad takes report 0x11, the effect at byte 6.
/// A clone takes the USB effect offsets instead, but Windows refuses a report ID the pad's
/// Bluetooth collection doesn't declare (`ERROR_INVALID_PARAMETER`), so which packet carries them
/// differs: most clones declare 0x11 like Sony's, some declare 0x05.
#[cfg(not(target_os = "ios"))]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Report {
    /// Sony's 0x11 report, effect at 6: [`dualshock::bluetooth`].
    Sony,
    /// A clone that declares 0x11: that report with the USB offsets inside
    /// ([`dualshock::usb_layout_in_bluetooth`]).
    UsbInSony,
    /// A clone that declares 0x05: the USB report with the Bluetooth CRC
    /// ([`dualshock::usb_over_bluetooth`]).
    Usb,
}

#[cfg(not(target_os = "ios"))]
enum Backend {
    Hid(HidDevice),
    #[cfg(windows)]
    Wgi(wgi::Pad),
}

#[cfg(not(target_os = "ios"))]
impl Xbox {
    /// The pad `target` names, on the link it is on. Windows.Gaming.Input says which link the
    /// pad is on: a wired pad goes through it (its HID endpoint takes no report), and a paired
    /// Bluetooth entry that Windows still lists after the cable goes in can't win. A pad on
    /// Bluetooth takes its own HID report when that opens, as on Linux and macOS, and WGI
    /// otherwise. A DualShock 4 never goes through WGI: its HID endpoint takes reports over both
    /// links, so it is opened directly. [`Target::ANY`] keeps the old choice, the first
    /// Microsoft Xbox One pad; a third-party target reaches WGI even though its ids aren't
    /// Microsoft's. `None` when there is none, an error when it can't be opened (Linux without
    /// access to its hidraw node).
    /// `HORIZON_CONTROLLER_LOG` logs what opened, once per pad.
    pub fn open(api: &HidApi, target: Target) -> Option<Result<Self>> {
        let opened = Self::open_at(api, target);
        if std::env::var_os("HORIZON_CONTROLLER_LOG").is_some() {
            let who = if target == Target::ANY {
                "the first pad".to_string()
            } else {
                format!(
                    "{:04x}:{:04x} #{}",
                    target.vendor, target.product, target.index
                )
            };
            match &opened {
                Some(Ok(xbox)) => log::info!(
                    "Horizon: controller: pad {who}: {:?} over {:?}",
                    xbox.frame,
                    xbox.transport
                ),
                Some(Err(e)) => log::info!("Horizon: controller: pad {who} did not open: {e}"),
                None => {
                    // Scanning runs every few seconds; say "no pad" once, not each time.
                    static NO_PAD: AtomicBool = AtomicBool::new(false);
                    if !NO_PAD.swap(true, Ordering::Relaxed) {
                        log::info!("Horizon: controller: no pad for {who}");
                    }
                }
            }
        }
        opened
    }

    fn open_at(api: &HidApi, target: Target) -> Option<Result<Self>> {
        static SKIPPED: AtomicBool = AtomicBool::new(false);
        let transport = |d: &hidapi::DeviceInfo| match d.bus_type() {
            BusType::Bluetooth => Transport::Bluetooth,
            _ => Transport::Usb,
        };
        let frame = if dualshock::is_dualshock(target.vendor, target.product) {
            Frame::Dualshock
        } else {
            Frame::Gip
        };
        // The matches, wired first: a pad on a cable can still be listed over Bluetooth for a
        // moment. `index` picks among identical pads of this model. The gamepad collection leads
        // them all: a DualShock 4 also lists a vendor-specific HID interface, and a report written
        // to that one goes nowhere (SDL filters on the same pair).
        let mut pads: Vec<&hidapi::DeviceInfo> = api
            .device_list()
            .filter(|d| {
                let ids = (d.vendor_id(), d.product_id());
                let known = match frame {
                    Frame::Dualshock => dualshock::is_dualshock(ids.0, ids.1),
                    Frame::Gip => is_xbox_one(ids.0, ids.1),
                };
                known && target.matches(ids.0, ids.1)
            })
            .collect();
        pads.sort_by_key(|d| {
            let gamepad = d.usage_page() == 1 && d.usage() == 5;
            (!gamepad, transport(d) == Transport::Bluetooth)
        });
        let rank = if target == Target::ANY {
            0
        } else {
            target.index
        };
        let chosen = pads.get(rank).or_else(|| pads.first()).copied();
        // A DualShock 4's wired endpoint takes reports; an Xbox One pad's wired one is XGIP's
        // stand-in, which takes none (hence WGI).
        let takes_report = |transport| frame == Frame::Dualshock || output_supported(transport);
        let skipped = chosen.is_some_and(|info| !takes_report(transport(info)));
        if skipped && !SKIPPED.swap(true, Ordering::Relaxed) {
            log::info!("Horizon: a wired Xbox pad has no output on this system");
        }
        #[cfg(windows)]
        if frame == Frame::Gip {
            match wgi::Pad::open(target) {
                // A cable: WGI is the only way to its trigger motors.
                Ok(Some(pad)) if !pad.wireless() => {
                    return Some(Ok(Xbox {
                        backend: Backend::Wgi(pad),
                        transport: Transport::Usb,
                        frame,
                        report: Report::Sony,
                    }));
                }
                // Bluetooth: the pad's own report is the proven path for its triggers. WGI takes
                // the pad when that endpoint can't be opened (another process holds it).
                Ok(Some(pad)) => {
                    if let Some(info) = chosen
                        && transport(info) == Transport::Bluetooth
                        && let Ok(device) = info.open_device(api)
                    {
                        log_open(info, frame, Transport::Bluetooth, Report::Sony);
                        return Some(Ok(Xbox {
                            backend: Backend::Hid(device),
                            transport: Transport::Bluetooth,
                            frame,
                            report: Report::Sony,
                        }));
                    }
                    // Another process holds the endpoint (the launcher): WGI takes the pad.
                    return Some(Ok(Xbox {
                        backend: Backend::Wgi(pad),
                        transport: Transport::Bluetooth,
                        frame,
                        report: Report::Sony,
                    }));
                }
                Ok(None) => {}
                Err(e) => log::debug!("Horizon: Windows.Gaming.Input: {e}"),
            }
        }
        if let Some(info) = chosen {
            let transport = transport(info);
            if takes_report(transport) {
                match info.open_device(api) {
                    Ok(device) => {
                        // Only a pad on Bluetooth answers this differently: over a cable both
                        // kinds take the USB report. A pad that isn't Sony's reads the USB
                        // offsets; most of those also declare 0x11 on Bluetooth (Windows refuses
                        // the 0x05 report there), and a few declare 0x05.
                        //
                        // The clone reading is a Windows observation (DS4Windows takes a Bluetooth
                        // pad's serial from `HidD_GetSerialNumberString`, and that is what a clone
                        // differs in), so it stays there: elsewhere a genuine pad whose serial the
                        // backend doesn't expose keeps Sony's report, as it had.
                        let mut report = Report::Sony;
                        #[cfg(windows)]
                        if frame == Frame::Dualshock
                            && transport == Transport::Bluetooth
                            && !is_sonys_dualshock(info.serial_number())
                        {
                            report = Report::UsbInSony;
                        }
                        // A pad that copies a serial too is the one guess that can go wrong:
                        // `HORIZON_CONTROLLER_REPORT=btusb|usb|sony` settles it without a build.
                        match std::env::var("HORIZON_CONTROLLER_REPORT").as_deref() {
                            Ok("btusb") if frame == Frame::Dualshock => report = Report::UsbInSony,
                            Ok("usb") if frame == Frame::Dualshock => report = Report::Usb,
                            Ok("sony") if frame == Frame::Dualshock => report = Report::Sony,
                            _ => {}
                        }
                        log_open(info, frame, transport, report);
                        #[cfg(windows)]
                        if std::env::var_os("HORIZON_CONTROLLER_PROBE").is_some()
                            && frame == Frame::Dualshock
                            && transport == Transport::Bluetooth
                        {
                            probe_bluetooth(&device);
                        }
                        return Some(Ok(Xbox {
                            backend: Backend::Hid(device),
                            transport,
                            frame,
                            report,
                        }));
                    }
                    Err(e) => return Some(Err(e.into())),
                }
            }
        }
        None
    }

    pub fn transport(&self) -> Transport {
        self.transport
    }

    /// `light`: the colour a DualShock 4's lightbar shows, once the game has one; an Xbox One pad
    /// leaves it alone. `HORIZON_CONTROLLER_LOG` logs each report's four levels: what the game
    /// asked for, before a GIP report turns the handles' bytes into 0..100.
    pub fn write(&mut self, motors: Motors, light: Option<[u8; 3]>) -> Result<()> {
        if std::env::var_os("HORIZON_CONTROLLER_LOG").is_some() {
            log::info!(
                "Horizon: controller: {:?} over {:?}: handles {}/{} (GIP {}/100 {}), triggers {}/{}, light {:?}",
                self.frame,
                self.transport,
                motors.strong,
                motors.weak,
                magnitude(motors.strong),
                magnitude(motors.weak),
                motors.left_trigger,
                motors.right_trigger,
                light,
            );
        }
        match &mut self.backend {
            Backend::Hid(device) => match (self.frame, self.transport) {
                (Frame::Gip, Transport::Usb) => {
                    device.write(&usb(motors))?;
                }
                (Frame::Gip, Transport::Bluetooth) => {
                    device.write(&bluetooth(motors))?;
                }
                // The trigger levels are dropped: a DualShock 4 has no trigger motors. Over
                // Bluetooth the report takes the interrupt pipe first: see `write_bluetooth`.
                (Frame::Dualshock, Transport::Usb) => {
                    device.write(&dualshock::usb(motors, light))?;
                }
                // A pad that isn't Sony's reads the USB offsets. Windows refuses a report ID the
                // pad's Bluetooth collection doesn't declare (`ERROR_INVALID_PARAMETER`), so a
                // clone that declares only 0x11 takes that report with the USB layout inside it,
                // and one that declares 0x05 takes the USB report with the CRC. A refusal by the
                // stack says which the pad declares: the other packet is tried, once, and kept.
                (Frame::Dualshock, Transport::Bluetooth) => match self.report {
                    Report::Sony => {
                        write_bluetooth(device, &dualshock::bluetooth(motors, light))?;
                    }
                    Report::UsbInSony => {
                        let layout = dualshock::usb_layout_in_bluetooth(motors, light);
                        if let Err(first) = write_bluetooth(device, &layout) {
                            let usb = dualshock::usb_over_bluetooth(motors, light);
                            write_bluetooth(device, &usb).map_err(|_| first)?;
                            self.report = Report::Usb;
                            log::info!(
                                "Horizon: controller: this pad's Bluetooth collection declares report 0x05, not 0x11: it takes the USB report with the CRC"
                            );
                        }
                    }
                    Report::Usb => {
                        let usb = dualshock::usb_over_bluetooth(motors, light);
                        if let Err(first) = write_bluetooth(device, &usb) {
                            let layout = dualshock::usb_layout_in_bluetooth(motors, light);
                            write_bluetooth(device, &layout).map_err(|_| first)?;
                            self.report = Report::UsbInSony;
                            log::info!(
                                "Horizon: controller: this pad's Bluetooth collection declares report 0x11, not 0x05: it takes Sony's report with the USB layout inside it"
                            );
                        }
                    }
                },
            },
            #[cfg(windows)]
            Backend::Wgi(pad) => pad.write(motors)?,
        };
        Ok(())
    }
}

/// A report over Bluetooth. The interrupt-out pipe is the one a DualShock 4 listens to, as SDL
/// (`SDL_hidapi_ps4.c`) and DS4Windows (`DS4Device.writeOutput`) send it. The control pipe
/// (`send_output_report`, Set_Report) is the fallback for a stack that refuses the interrupt
/// write: a report there can be accepted and never delivered, which is what a player's DualShock 4
/// over Bluetooth showed — every Set_Report took, and the pad stayed silent. (The interrupt pipe
/// has its own silent-drop report on some adapters, libusb/hidapi#513, which surfaces as `Ok`, so
/// the fallback cannot see it: `HORIZON_CONTROLLER_PIPE=control` takes the control pipe first for
/// a pad that comes out silent there.)
#[cfg(windows)]
fn write_bluetooth(device: &HidDevice, report: &[u8]) -> std::result::Result<(), HidError> {
    if std::env::var("HORIZON_CONTROLLER_PIPE").as_deref() == Ok("control") {
        return device.send_output_report(report);
    }
    send_bluetooth(
        report,
        |r| device.write(r).map(|_| ()),
        |r| device.send_output_report(r),
    )
}

/// The interrupt-out pipe first, the control pipe only if it fails: a report the first one takes
/// is not written twice. The error that comes back is the interrupt pipe's, the one that says what
/// went wrong with the pad. The closures name no platform, so the tests below run everywhere.
#[cfg(any(windows, test))]
fn send_bluetooth<E: std::fmt::Display>(
    report: &[u8],
    interrupt: impl FnOnce(&[u8]) -> std::result::Result<(), E>,
    control: impl FnOnce(&[u8]) -> std::result::Result<(), E>,
) -> std::result::Result<(), E> {
    match interrupt(report) {
        Ok(()) => Ok(()),
        Err(interrupt) => match control(report) {
            Ok(()) => {
                // A stack that needs the control pipe is unusual: say so once, not per report.
                static FELL_BACK: AtomicBool = AtomicBool::new(false);
                if !FELL_BACK.swap(true, Ordering::Relaxed) {
                    log::info!(
                        "Horizon: controller: a Bluetooth report took the control pipe: {interrupt}"
                    );
                }
                Ok(())
            }
            Err(_) => Err(interrupt),
        },
    }
}

/// Everywhere else the interrupt-out pipe is the path hidapi and SDL take, and it delivers.
#[cfg(not(windows))]
fn write_bluetooth(device: &HidDevice, report: &[u8]) -> std::result::Result<(), HidError> {
    device.write(report).map(|_| ())
}

/// A one-shot diagnostic for a DualShock 4 on Bluetooth, when `HORIZON_CONTROLLER_PROBE` is set:
/// four reports, one per pair of layout and pipe, each buzzing for a second and a half and setting
/// a distinct lightbar colour, then the motors stop. The player says which step did something and
/// the log says which step was which, so "no vibration over Bluetooth" stops being a guess about
/// which of the two the pad wants. It runs once, as the pad opens, before the game writes anything.
#[cfg(windows)]
fn probe_bluetooth(device: &HidDevice) {
    log::info!(
        "Horizon: controller: probe: four reports, 1.5 s each. Note which one buzzes or changes the lightbar: 1 = red, 2 = green, 3 = blue, 4 = white"
    );
    let steps = [
        (false, false, [255, 0, 0]),
        (false, true, [0, 255, 0]),
        (true, false, [0, 0, 255]),
        (true, true, [255, 255, 255]),
    ];
    for (index, (usb_layout, control_pipe, colour)) in steps.into_iter().enumerate() {
        let step = index + 1;
        let what = match (usb_layout, control_pipe) {
            (false, false) => "Sony's report on the interrupt pipe",
            (false, true) => "Sony's report on the control pipe",
            (true, false) => "the USB layout on the interrupt pipe",
            _ => "the USB layout on the control pipe",
        };
        log::info!("Horizon: controller: probe {step}/4: {what}, lightbar {colour:?}");
        let buzz = Motors {
            strong: 255,
            weak: 255,
            left_trigger: 0,
            right_trigger: 0,
        };
        let sent = probe_write(device, buzz, colour, usb_layout, control_pipe);
        log::info!("Horizon: controller: probe {step}/4: write -> {sent:?}");
        std::thread::sleep(Duration::from_millis(1500));
        let stopped = probe_write(device, Motors::STOP, colour, usb_layout, control_pipe);
        log::info!("Horizon: controller: probe {step}/4: stop -> {stopped:?}");
        std::thread::sleep(Duration::from_millis(1500));
    }
    log::info!("Horizon: controller: probe: done");
}

/// One probe report, on the pipe and in the layout the caller names.
#[cfg(windows)]
fn probe_write(
    device: &HidDevice,
    motors: Motors,
    colour: [u8; 3],
    usb_layout: bool,
    control_pipe: bool,
) -> std::result::Result<(), HidError> {
    if usb_layout {
        let report = dualshock::usb_layout_in_bluetooth(motors, Some(colour));
        if control_pipe {
            device.send_output_report(&report)
        } else {
            device.write(&report).map(|_| ())
        }
    } else {
        let report = dualshock::bluetooth(motors, Some(colour));
        if control_pipe {
            device.send_output_report(&report)
        } else {
            device.write(&report).map(|_| ())
        }
    }
}

/// Which collection the reports go to, when `HORIZON_CONTROLLER_LOG` asks: a DualShock 4 lists a
/// gamepad collection (usage page 1, usage 5) and a vendor-specific one, and only the gamepad one
/// carries output. A DualShock 4 on Bluetooth also gets a line naming the pad, whether it lists a
/// serial, and which report it was given: that is the question a "no vibration over Bluetooth"
/// report turns on.
fn log_open(info: &hidapi::DeviceInfo, frame: Frame, transport: Transport, report: Report) {
    if std::env::var_os("HORIZON_CONTROLLER_LOG").is_none() {
        return;
    }
    log::info!(
        "Horizon: controller: opened {:04x}:{:04x} usage page {} usage {} interface {} as {:?} over {:?}",
        info.vendor_id(),
        info.product_id(),
        info.usage_page(),
        info.usage(),
        info.interface_number(),
        frame,
        transport,
    );
    if frame == Frame::Dualshock && transport == Transport::Bluetooth {
        let serial = info
            .serial_number()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let what = match report {
            Report::Sony => "Sony's 0x11 report",
            Report::UsbInSony => "Sony's 0x11 report with the USB layout inside it",
            Report::Usb => "the USB report with the CRC",
        };
        // The serial, its maker and the verdict the classifier takes from it, side by side: a clone
        // that copies Sony's ids leaves the serial empty or unlike a pad's address, and that is what
        // decides the report above (`is_sonys_dualshock`). On Windows the verdict chose the report;
        // elsewhere the report stays Sony's and this line only says what the serial looks like.
        log::info!(
            "Horizon: controller: the pad lists {:?} by {:?} serial {:?} (Sony's: {}): it takes {what} over Bluetooth",
            info.product_string(),
            info.manufacturer_string(),
            serial,
            is_sonys_dualshock(serial),
        );
        if let Ok(forced) = std::env::var("HORIZON_CONTROLLER_REPORT")
            && !forced.is_empty()
        {
            log::info!("Horizon: controller: HORIZON_CONTROLLER_REPORT={forced} chose that report");
        }
    }
}

/// Whether a DualShock 4 is Sony's, from the serial its descriptor lists: DS4Windows'
/// `ReadSerial` takes a Bluetooth pad's from there (`HidD_GetSerialNumberString`), and Sony's
/// pads list theirs. A pad that lists none is one of the clones that only know the USB report,
/// which is where Windows lists it as a generic "HID-compliant game controller". A pad that
/// copies a serial too is why `HORIZON_CONTROLLER_REPORT` exists.
fn is_sonys_dualshock(serial: Option<&str>) -> bool {
    serial.is_some_and(|serial| {
        let digits: Vec<char> = serial.chars().filter(char::is_ascii_hexdigit).collect();
        digits.len() >= 6 && digits.iter().any(|&digit| digit != '0')
    })
}

/// A vibration: the motors' levels, each for a while, in order.
pub type Rumble = Vec<(Motors, Duration)>;

// On iOS no thread reads the queue (the Link never opens); the variants stay so the
// Link's API is the same everywhere.
#[cfg_attr(target_os = "ios", allow(dead_code))]
enum Message {
    Rumble(Rumble),
    /// The lightbar's colour (a DualShock 4's); `None` stops setting one.
    Light(Option<[u8; 3]>),
    Scan,
}

/// The output thread. Dropping it stops the motors before it ends; on iOS there is none (no
/// hidapi backend) and the Link stays closed.
pub struct Link {
    tx: Option<Sender<Message>>,
    /// 0: no pad open, else 1 + the transport.
    open: Arc<AtomicU8>,
    thread: Option<JoinHandle<()>>,
}

impl Link {
    /// Start the thread and look for the pad `target` names.
    #[cfg(not(target_os = "ios"))]
    pub fn start(target: Target) -> Link {
        let (tx, rx) = mpsc::channel();
        let open = Arc::new(AtomicU8::new(0));
        let shown = open.clone();
        let thread = std::thread::Builder::new()
            .name("xbox".into())
            .spawn(move || Writer::new(shown, target).run(rx))
            .map_err(|e| log::warn!("Horizon: Xbox output: {e}"))
            .ok();
        Link {
            tx: Some(tx),
            open,
            thread,
        }
    }

    /// No output on iOS: hidapi has no backend there. The Link never opens.
    #[cfg(target_os = "ios")]
    pub fn start(_target: Target) -> Link {
        Link {
            tx: None,
            open: Arc::new(AtomicU8::new(0)),
            thread: None,
        }
    }

    /// The pad's transport while one is open and taking reports.
    pub fn open(&self) -> Option<Transport> {
        match self.open.load(Ordering::Relaxed) {
            1 => Some(Transport::Usb),
            2 => Some(Transport::Bluetooth),
            _ => None,
        }
    }

    /// Look for the pad again (a controller connected).
    pub fn scan(&self) {
        self.send(Message::Scan);
    }

    /// Play a vibration, replacing the one playing.
    pub fn rumble(&self, rumble: Rumble) {
        self.send(Message::Rumble(rumble));
    }

    /// Show `colour` on a DualShock 4's lightbar, or `None` to stop setting one. A colour is kept
    /// and re-sent with every later report, and again when the pad comes back; `None` drops it, so
    /// later reports leave the lightbar as the pad has it. An Xbox One pad ignores it.
    pub fn light(&self, colour: Option<[u8; 3]>) {
        self.send(Message::Light(colour));
    }

    fn send(&self, message: Message) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(message);
        }
    }
}

impl Drop for Link {
    fn drop(&mut self) {
        // Closing the channel ends the thread, which stops the motors first.
        self.tx = None;
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Looks for the pad this many times, a second apart, after a scan finds nothing (its hidraw
/// node may appear after the controller does).
#[cfg(not(target_os = "ios"))]
const SCAN_TRIES: u32 = 5;

#[cfg(not(target_os = "ios"))]
struct Writer {
    api: Option<HidApi>,
    pad: Option<Box<dyn Handle>>,
    open: Arc<AtomicU8>,
    /// The pad this writer drives, not just the first one there is.
    target: Target,
    rumble: Rumble,
    /// When the vibration's current step ends.
    step_end: Option<Instant>,
    /// The motors last written; `None` after (re)opening.
    sent: Option<Motors>,
    /// The DualShock 4 lightbar's colour, once the game set one: every report carries it, and a
    /// pad that comes back gets it again.
    light: Option<[u8; 3]>,
    scans_left: u32,
    next_scan: Instant,
}

/// What the writer needs from an open pad. It is a trait so the state machine can be tested
/// without hardware.
#[cfg(not(target_os = "ios"))]
trait Handle: Send {
    fn write(&mut self, motors: Motors, light: Option<[u8; 3]>) -> std::result::Result<(), String>;
}

#[cfg(not(target_os = "ios"))]
impl Handle for Xbox {
    fn write(&mut self, motors: Motors, light: Option<[u8; 3]>) -> std::result::Result<(), String> {
        Xbox::write(self, motors, light).map_err(|e| e.to_string())
    }
}

#[cfg(not(target_os = "ios"))]
impl Writer {
    fn new(open: Arc<AtomicU8>, target: Target) -> Self {
        Writer {
            api: None,
            pad: None,
            open,
            target,
            rumble: Vec::new(),
            step_end: None,
            sent: None,
            light: None,
            scans_left: 1,
            next_scan: Instant::now(),
        }
    }

    fn run(mut self, rx: mpsc::Receiver<Message>) {
        loop {
            let now = Instant::now();
            if self.pad.is_none() && self.scans_left > 0 && now >= self.next_scan {
                self.scan();
            }
            let wake = [
                self.step_end,
                (self.pad.is_none() && self.scans_left > 0).then_some(self.next_scan),
            ]
            .into_iter()
            .flatten()
            .min();
            let message = match wake {
                Some(at) => rx.recv_timeout(at.saturating_duration_since(now)),
                None => rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
            };
            match message {
                Ok(Message::Rumble(rumble)) => {
                    self.rumble = rumble;
                    self.rumble.reverse();
                    self.step_end = None;
                }
                Ok(Message::Light(colour)) => self.set_light(colour),
                Ok(Message::Scan) => {
                    // A pad that was unplugged while idle is found again by a write that fails;
                    // a hidapi zero-timeout read is not a liveness check on Windows (its overlapped
                    // read reports ERROR_BUSY instead of "no report yet"), so keep the handle and
                    // let the write judge it.
                    if self.pad.is_none() {
                        self.scans_left = SCAN_TRIES;
                        self.next_scan = Instant::now();
                    }
                    continue;
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            self.advance_rumble();
            self.flush();
        }
        if let Some(pad) = self.pad.as_mut() {
            let _ = pad.write(Motors::STOP, self.light);
        }
    }

    fn scan(&mut self) {
        self.scans_left -= 1;
        self.next_scan = Instant::now() + Duration::from_secs(1);
        let api = match self.api.as_mut() {
            Some(api) => api.refresh_devices().map(|()| &*api),
            None => HidApi::new().map(|api| &*self.api.insert(api)),
        };
        let found = match api {
            Ok(api) => Xbox::open(api, self.target),
            Err(e) => {
                log::warn!("Horizon: Xbox output: {e}");
                None
            }
        };
        match found {
            Some(Ok(pad)) => {
                log::info!("Xbox output over {:?}", pad.transport());
                self.open
                    .store(1 + pad.transport() as u8, Ordering::Relaxed);
                self.pad = Some(Box::new(pad));
                self.sent = None;
                self.scans_left = 0;
                self.flush();
            }
            Some(Err(e)) => {
                // No access (Linux without a udev rule): vibration stays with gilrs.
                log::info!("Horizon: the Xbox pad can't be opened for output: {e}");
                self.scans_left = 0;
            }
            None => {}
        }
    }

    /// Keep a new lightbar colour (`None`: stop setting one): the motors may not have changed, so
    /// the next report is forced to carry it, or to leave the lightbar alone.
    fn set_light(&mut self, colour: Option<[u8; 3]>) {
        if self.light != colour {
            self.light = colour;
            self.sent = None;
        }
    }

    /// Move to the vibration's next step once this one is over.
    fn advance_rumble(&mut self) {
        let now = Instant::now();
        match self.step_end {
            Some(end) if end > now => return,
            Some(_) => {
                self.rumble.pop();
            }
            None => {}
        }
        self.step_end = self.rumble.last().map(|&(_, length)| now + length);
    }

    /// Send what changed; a failed write means the pad is gone.
    fn flush(&mut self) {
        if self.pad.is_none() {
            return;
        }
        let motors = self
            .rumble
            .last()
            .map_or(Motors::STOP, |&(motors, _)| motors);
        if self.sent == Some(motors) {
            return;
        }
        if self.write(motors) {
            self.sent = Some(motors);
        }
    }

    /// Write a report, dropping the pad on any error: a handle that failed is never written
    /// through again. A zero-timeout read is *not* used as a liveness check first: on Windows
    /// hidapi's overlapped read reports `ERROR_BUSY` (0xAA) whenever no input report is ready,
    /// which closed the link on the first vibration over Bluetooth. hidapi's write returns the
    /// error itself, so a stale handle is dropped then.
    fn write(&mut self, motors: Motors) -> bool {
        let result = match self.pad.as_mut() {
            Some(pad) => pad.write(motors, self.light),
            None => return false,
        };
        match result {
            Ok(()) => true,
            Err(e) => {
                self.lost(&e);
                false
            }
        }
    }
    /// Drop a handle that hidapi failed on, and look for the pad again: one that slept or was
    /// unplugged may come back. Nothing is written through the old handle. The scan waits a second
    /// like every other one: a pad that refuses every report would otherwise be opened, written to
    /// and dropped thousands of times a second.
    fn lost(&mut self, why: &str) {
        log::info!("Xbox output closed: {why}");
        self.pad = None;
        self.sent = None;
        self.open.store(0, Ordering::Relaxed);
        self.scans_left = SCAN_TRIES;
        self.next_scan = Instant::now() + Duration::from_secs(1);
    }
}

#[cfg(all(test, not(target_os = "ios")))]
mod tests {
    use super::*;

    /// A pad whose writes can fail, so the writer's state machine runs without hardware.
    struct FakePad {
        writes: Arc<std::sync::Mutex<Vec<(Motors, Option<[u8; 3]>)>>>,
        write_fails: bool,
    }

    impl Handle for FakePad {
        fn write(
            &mut self,
            motors: Motors,
            light: Option<[u8; 3]>,
        ) -> std::result::Result<(), String> {
            self.writes.lock().unwrap().push((motors, light));
            if self.write_fails {
                Err("gone".into())
            } else {
                Ok(())
            }
        }
    }

    fn writer(
        open: &Arc<AtomicU8>,
        write_fails: bool,
    ) -> (
        Writer,
        Arc<std::sync::Mutex<Vec<(Motors, Option<[u8; 3]>)>>>,
    ) {
        let writes = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut writer = Writer::new(open.clone(), Target::ANY);
        writer.pad = Some(Box::new(FakePad {
            writes: writes.clone(),
            write_fails,
        }));
        (writer, writes)
    }

    #[test]
    fn a_failed_write_drops_the_pad_and_schedules_a_rescan() {
        let open = Arc::new(AtomicU8::new(1));
        let (mut writer, writes) = writer(&open, true);
        writer.flush();
        assert_eq!(writes.lock().unwrap().len(), 1);
        assert!(writer.pad.is_none());
        assert_eq!(open.load(Ordering::Relaxed), 0);
        assert!(writer.sent.is_none());
        assert_eq!(writer.scans_left, SCAN_TRIES);
        // The next scan waits like any other: a pad that refuses every report is not opened
        // thousands of times a second.
        assert!(writer.next_scan > Instant::now() + Duration::from_millis(500));
        // The handle that failed is not written through again.
        writer.flush();
        assert_eq!(writes.lock().unwrap().len(), 1);
    }

    #[test]
    fn a_live_pad_takes_the_motors_and_then_stops() {
        let open = Arc::new(AtomicU8::new(1));
        let (mut writer, writes) = writer(&open, false);
        let motors = Motors::levels(0.5, 0.25, [1.0, 0.0]);
        writer.rumble = vec![
            (Motors::STOP, Duration::from_millis(10)),
            (motors, Duration::from_millis(35)),
        ];
        writer.flush();
        assert_eq!(*writes.lock().unwrap(), vec![(motors, None)]);
        // The step is over: the writer stops the motors, and sends nothing more while stopped.
        writer.step_end = Some(Instant::now() - Duration::from_millis(1));
        writer.advance_rumble();
        writer.flush();
        writer.flush();
        assert_eq!(
            *writes.lock().unwrap(),
            vec![(motors, None), (Motors::STOP, None)]
        );
        assert!(writer.pad.is_some());
        assert_eq!(open.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn a_vibration_replaces_the_one_playing() {
        let open = Arc::new(AtomicU8::new(1));
        let (mut writer, writes) = writer(&open, false);
        writer.rumble = vec![(
            Motors::levels(0.2, 0.0, [0.0; 2]),
            Duration::from_millis(100),
        )];
        writer.flush();
        writer.rumble = vec![(
            Motors::levels(0.9, 0.0, [0.0; 2]),
            Duration::from_millis(100),
        )];
        writer.step_end = Some(Instant::now() + Duration::from_millis(100));
        writer.flush();
        assert_eq!(
            *writes.lock().unwrap(),
            vec![
                (Motors::levels(0.2, 0.0, [0.0; 2]), None),
                (Motors::levels(0.9, 0.0, [0.0; 2]), None),
            ]
        );
    }

    /// A lightbar colour goes out with the motors, is not repeated while it holds, and comes back
    /// with the first report after the pad did.
    #[test]
    fn the_lightbar_is_sent_once_and_again_when_the_pad_comes_back() {
        let open = Arc::new(AtomicU8::new(1));
        let (mut writer, writes) = writer(&open, false);
        writer.set_light(Some([0x11, 0x9d, 0x2e]));
        writer.flush();
        assert_eq!(
            *writes.lock().unwrap(),
            vec![(Motors::STOP, Some([0x11, 0x9d, 0x2e]))]
        );
        // The same colour, and a frame with no news: nothing is written again.
        writer.set_light(Some([0x11, 0x9d, 0x2e]));
        writer.flush();
        assert_eq!(writes.lock().unwrap().len(), 1);
        // Unplugged and plugged back: the new pad is told the colour with its first report.
        writer.lost("gone");
        let again = Arc::new(std::sync::Mutex::new(Vec::new()));
        writer.pad = Some(Box::new(FakePad {
            writes: again.clone(),
            write_fails: false,
        }));
        writer.flush();
        assert_eq!(
            *again.lock().unwrap(),
            vec![(Motors::STOP, Some([0x11, 0x9d, 0x2e]))]
        );
    }

    /// Turning the lightbar setting off drops the kept colour: one report leaves the lightbar
    /// alone, and so does every rumble after it.
    #[test]
    fn turning_the_lightbar_off_drops_the_kept_colour() {
        let open = Arc::new(AtomicU8::new(1));
        let (mut writer, writes) = writer(&open, false);
        writer.set_light(Some([0x11, 0x9d, 0x2e]));
        writer.flush();
        writer.set_light(None);
        writer.flush();
        writer
            .rumble
            .push((Motors::levels(0.5, 0.0, [0.0; 2]), Duration::from_secs(1)));
        writer.flush();
        assert_eq!(
            *writes.lock().unwrap(),
            vec![
                (Motors::STOP, Some([0x11, 0x9d, 0x2e])),
                (Motors::STOP, None),
                (Motors::levels(0.5, 0.0, [0.0; 2]), None),
            ]
        );
    }

    /// The DualShock 4's Bluetooth report takes the interrupt-out pipe first, the one SDL and
    /// DS4Windows write; a report it takes is not also sent over the control pipe.
    #[test]
    fn a_bluetooth_report_takes_the_interrupt_pipe_first() {
        let calls = std::cell::RefCell::new(Vec::new());
        let result = send_bluetooth(
            b"report",
            |report| {
                calls.borrow_mut().push(("interrupt", report.to_vec()));
                Ok::<(), &str>(())
            },
            |report| {
                calls.borrow_mut().push(("control", report.to_vec()));
                Ok::<(), &str>(())
            },
        );
        assert_eq!(result, Ok(()));
        assert_eq!(*calls.borrow(), vec![("interrupt", b"report".to_vec())]);
    }

    /// An interrupt write the stack refuses hands the same report to the control pipe, and that
    /// still counts as delivered: the pad stays open.
    #[test]
    fn a_refused_interrupt_write_falls_back_to_the_control_pipe() {
        let calls = std::cell::RefCell::new(Vec::new());
        let result = send_bluetooth(
            b"report",
            |report| {
                calls.borrow_mut().push(("interrupt", report.to_vec()));
                Err::<(), &str>("the endpoint is busy")
            },
            |report| {
                calls.borrow_mut().push(("control", report.to_vec()));
                Ok::<(), &str>(())
            },
        );
        assert_eq!(result, Ok(()));
        assert_eq!(
            *calls.borrow(),
            vec![
                ("interrupt", b"report".to_vec()),
                ("control", b"report".to_vec()),
            ]
        );
    }

    /// Sony's pad lists a serial; the pad a player reported — a generic "HID-compliant game
    /// controller" in Windows, with none — does not, and so takes the USB report over Bluetooth.
    #[test]
    fn a_dualshock_without_a_serial_is_not_sonys() {
        assert!(is_sonys_dualshock(Some("a0ab51c2d3e4")));
        assert!(!is_sonys_dualshock(None));
        assert!(!is_sonys_dualshock(Some("")));
        assert!(!is_sonys_dualshock(Some("00:00:00:00:00:00")));
    }
}
