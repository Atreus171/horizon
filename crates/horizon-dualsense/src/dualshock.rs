//! A DualShock 4's output: the two handles and the lightbar, written as SDL's
//! `SDL_hidapi_ps4.c` writes them (docs/controllers.md, "Xbox One and DualShock 4 output"). The pad
//! has no trigger motors and no HD haptics, so the trigger levels are dropped.
use crate::xbox::Motors;

/// Sony's vendor ID (the same as the DualSense's, in [`crate::SONY`]).
pub const SONY: u16 = crate::SONY;
/// The DualShock 4's product IDs: the launch pad (CUH-ZCT1) and the slim one (CUH-ZCT2). The
/// wireless adapter's id (0x0ba0) is left out: it takes reports of its own.
pub const DUALSHOCK_IDS: [u16; 2] = [0x05c4, 0x09cc];

/// Whether a USB vendor / product pair is a DualShock 4.
pub fn is_dualshock(vendor: u16, product: u16) -> bool {
    vendor == SONY && DUALSHOCK_IDS.contains(&product)
}

/// The effect block's first byte marks which of its fields the report sets.
const RUMBLE: u8 = 0x01;
const LIGHT: u8 = 0x02;
/// The blink bit a pad that isn't Sony's expects to hear about with the rest of the effect
/// (SDL's `k_EPS4EffectLEDBlink`, set in its third-party mask).
const BLINK: u8 = 0x04;
/// SDL's Bluetooth reports lead with `0xC0 | report_interval`; 4 ms is its default.
const BT_MAGIC: u8 = 0xc0 | 4;

pub const USB_REPORT_ID: u8 = 0x05;
pub const USB_REPORT_LEN: usize = 32;
pub const BT_REPORT_ID: u8 = 0x11;
pub const BT_REPORT_LEN: usize = 78;
/// The USB report with the Bluetooth CRC appended: 32 bytes, the USB report's own length, its last
/// four given to the CRC over the first 28 ([`usb_over_bluetooth`]).
pub const BT_USB_REPORT_LEN: usize = 32;
/// Both reports carry the effect at this offset (report ID included).
const USB_EFFECT: usize = 4;
const BT_EFFECT: usize = 6;

/// The mask for a report: the handles always, the lightbar when the game has a colour for it.
fn mask(light: Option<[u8; 3]>) -> u8 {
    RUMBLE | if light.is_some() { LIGHT } else { 0 }
}

/// The USB report: 0x05, the mask, then the effect at offset 4 — the light motor, the heavy one,
/// 0..255, and the lightbar when `light` is set, its red, green and blue (the flash delays stay
/// zero). The trigger motors' levels are dropped: the pad has none.
pub fn usb(motors: Motors, light: Option<[u8; 3]>) -> [u8; USB_REPORT_LEN] {
    let mut r = [0; USB_REPORT_LEN];
    r[0] = USB_REPORT_ID;
    r[1] = mask(light);
    r[USB_EFFECT] = motors.weak;
    r[USB_EFFECT + 1] = motors.strong;
    if let Some(rgb) = light {
        r[USB_EFFECT + 2..USB_EFFECT + 5].copy_from_slice(&rgb);
    }
    r
}

/// The Bluetooth report: 0x11, 0xC0 | the report interval, the mask at 3, the effect at 6, and
/// the CRC-32 of the seed byte 0xA2 and the first 74 bytes, little-endian.
pub fn bluetooth(motors: Motors, light: Option<[u8; 3]>) -> [u8; BT_REPORT_LEN] {
    let mut r = [0; BT_REPORT_LEN];
    r[0] = BT_REPORT_ID;
    r[1] = BT_MAGIC;
    r[3] = mask(light);
    r[BT_EFFECT] = motors.weak;
    r[BT_EFFECT + 1] = motors.strong;
    if let Some(rgb) = light {
        r[BT_EFFECT + 2..BT_EFFECT + 5].copy_from_slice(&rgb);
    }
    let crc = crate::bt_crc(&r[..BT_REPORT_LEN - 4]);
    r[BT_REPORT_LEN - 4..].copy_from_slice(&crc.to_le_bytes());
    r
}

/// The USB report with the Bluetooth CRC after it: what a DualShock 4 that isn't Sony's takes
/// over Bluetooth as well. Those pads know only the USB layout, and a 0x11 report leaves their
/// motors off and their lightbar blinking: read with the USB offsets, its byte 1 (`0xC0 | 4`) is
/// the mask, which has the blink bit (0x04) set and the motor bit (0x01) clear. SDL sends its
/// third-party branch the USB report on both links (`SDL_hidapi_ps4.c`,
/// `HIDAPI_DriverPS4_InternalSendJoystickEffect`), and DS4Windows calls the same pads
/// `OnlyOutputData0x05`.
///
/// The mask is SDL's third-party one for every report, the LED bit included — "Some third party
/// gamepads expect to receive rumble and LED effects at the same time" — so a game with no colour
/// sends black bytes and the bar goes dark. Leaving the bit out would make these pads ignore the
/// colour bytes and keep the colour they had, which on the pad this fixes is the flashing white it
/// is being cured of.
///
/// Only a pad whose Bluetooth collection declares report 0x05 takes this: Windows refuses a
/// report ID the collection does not list with `ERROR_INVALID_PARAMETER` (see
/// [`usb_layout_in_bluetooth`] for the pads that declare only Sony's 0x11).
pub fn usb_over_bluetooth(motors: Motors, light: Option<[u8; 3]>) -> [u8; BT_USB_REPORT_LEN] {
    let mut r = [0; BT_USB_REPORT_LEN];
    r[0] = USB_REPORT_ID;
    r[1] = RUMBLE | LIGHT | BLINK;
    r[USB_EFFECT] = motors.weak;
    r[USB_EFFECT + 1] = motors.strong;
    if let Some(rgb) = light {
        r[USB_EFFECT + 2..USB_EFFECT + 5].copy_from_slice(&rgb);
    }
    let crc = crate::bt_crc(&r[..BT_USB_REPORT_LEN - 4]);
    r[BT_USB_REPORT_LEN - 4..].copy_from_slice(&crc.to_le_bytes());
    r
}

/// Sony's 0x11 report with the **USB field offsets** inside it: the report ID and length a
/// Bluetooth DualShock 4 collection declares, filled the way a pad whose firmware only knows the
/// USB layout reads it. A clone's descriptor lists 0x11 (78 bytes) like Sony's, so Windows refuses
/// report 0x05 there with `ERROR_INVALID_PARAMETER`, but its parser still takes the mask from byte
/// 1 and the motors and colour from the USB offsets — that is what a player's `054c:05c4` showed
/// when Sony's layout left the lightbar blinking white and the handles still. The mask carries no
/// blink bit (the pad is asked to hold the colour, not flash it), and it carries the LED bit only
/// when the game has a colour: with none, the bar is left as it is, which cannot flash because the
/// blink bit is clear — unlike [`usb_over_bluetooth`], whose LED bit is SDL's and always set. The
/// CRC-32 of the seed byte 0xA2 and the first 74 bytes rides the end as every Bluetooth report
/// does.
pub fn usb_layout_in_bluetooth(motors: Motors, light: Option<[u8; 3]>) -> [u8; BT_REPORT_LEN] {
    let mut r = [0; BT_REPORT_LEN];
    r[0] = BT_REPORT_ID;
    r[1] = mask(light);
    r[USB_EFFECT] = motors.weak;
    r[USB_EFFECT + 1] = motors.strong;
    if let Some(rgb) = light {
        r[USB_EFFECT + 2..USB_EFFECT + 5].copy_from_slice(&rgb);
    }
    let crc = crate::bt_crc(&r[..BT_REPORT_LEN - 4]);
    r[BT_REPORT_LEN - 4..].copy_from_slice(&crc.to_le_bytes());
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_sonys_dualshocks_are_ours() {
        assert!(is_dualshock(SONY, 0x05c4));
        assert!(is_dualshock(SONY, 0x09cc));
        assert!(!is_dualshock(SONY, 0x0ce6), "a DualSense");
        assert!(!is_dualshock(0x045e, 0x09cc), "another maker's pad");
    }

    /// The layout SDL sends over USB: mask 0x01 at [1], the light motor at [4], the heavy at [5],
    /// and nothing in the 27 bytes after (the lightbar is not touched).
    #[test]
    fn the_usb_report_carries_the_two_handles() {
        let motors = Motors {
            strong: 200,
            weak: 40,
            left_trigger: 255,
            right_trigger: 255,
        };
        let mut expected = [0; USB_REPORT_LEN];
        expected[0] = 0x05;
        expected[1] = 0x01;
        expected[4] = 40;
        expected[5] = 200;
        assert_eq!(usb(motors, None), expected);
    }

    /// The lightbar's bytes follow the handles in the same effect block, and the mask says so:
    /// SDL's `DS4EffectsState_t` is rumbleRight, rumbleLeft, ledRed, ledGreen, ledBlue, delayOn,
    /// delayOff.
    #[test]
    fn the_lightbar_rides_the_same_effect_block() {
        let motors = Motors {
            strong: 200,
            weak: 40,
            ..Motors::STOP
        };
        let light = Some([0x11, 0x9d, 0x2e]);
        let mut expected = [0; USB_REPORT_LEN];
        expected[0] = 0x05;
        expected[1] = 0x03;
        expected[4] = 40;
        expected[5] = 200;
        expected[6..9].copy_from_slice(&[0x11, 0x9d, 0x2e]);
        assert_eq!(usb(motors, light), expected);

        let bt = bluetooth(motors, light);
        assert_eq!(
            &bt[..11],
            &[
                0x11, 0xc4, 0x00, 0x03, 0x00, 0x00, 40, 200, 0x11, 0x9d, 0x2e
            ]
        );
    }

    /// The layout SDL sends over Bluetooth, with its CRC of the seed 0xA2 and the first 74
    /// bytes (the expected CRC is zlib's crc32 over the same bytes, computed apart).
    #[test]
    fn the_bluetooth_report_ends_with_the_seeded_crc() {
        let report = bluetooth(
            Motors {
                strong: 200,
                weak: 40,
                left_trigger: 0,
                right_trigger: 0,
            },
            None,
        );
        assert_eq!(&report[..8], &[0x11, 0xc4, 0x00, 0x01, 0x00, 0x00, 40, 200]);
        assert!(report[8..74].iter().all(|&b| b == 0));
        assert_eq!(report[74..], [0x30, 0xb3, 0x23, 0xf6]);
    }

    /// The CRC covers the lightbar too: zlib's crc32 of the seed 0xA2 and the first 74 bytes of
    /// this report, computed apart.
    #[test]
    fn the_bluetooth_light_report_keeps_its_crc() {
        let report = bluetooth(
            Motors {
                strong: 200,
                weak: 40,
                ..Motors::STOP
            },
            Some([0x11, 0x9d, 0x2e]),
        );
        assert_eq!(report[74..], [0xad, 0x90, 0x17, 0xbf]);
    }

    /// A pad that isn't Sony's takes the USB report over Bluetooth too: 0x05, the mask with the
    /// blink bit, the handles at 4 and 5, the lightbar after them, and the CRC-32 of the seed
    /// 0xA2 and the first 28 bytes (the expected CRC is zlib's, computed apart).
    #[test]
    fn a_third_party_bluetooth_report_is_the_usb_one_with_the_crc() {
        let report = usb_over_bluetooth(
            Motors {
                strong: 200,
                weak: 40,
                ..Motors::STOP
            },
            Some([0x11, 0x9d, 0x2e]),
        );
        assert_eq!(
            &report[..11],
            &[
                0x05, 0x07, 0x00, 0x00, 40, 200, 0x11, 0x9d, 0x2e, 0x00, 0x00
            ]
        );
        assert!(report[11..28].iter().all(|&b| b == 0));
        assert_eq!(report[28..], [0x27, 0xc7, 0x0f, 0x6d]);
    }

    /// A game with no lightbar colour still sets the LED and blink bits, as SDL's third-party mask
    /// does: these pads expect both effects together, and the colour bytes are black, so the bar
    /// goes dark instead of keeping the colour it had (on the pad this fixes, that was the flashing
    /// white).
    #[test]
    fn a_third_party_bluetooth_report_without_a_light_sends_black() {
        let report = usb_over_bluetooth(
            Motors {
                strong: 200,
                weak: 40,
                ..Motors::STOP
            },
            None,
        );
        assert_eq!(&report[..6], &[0x05, 0x07, 0x00, 0x00, 40, 200]);
        assert!(report[6..28].iter().all(|&b| b == 0));
        assert_eq!(report[28..], [0xd6, 0xf3, 0x52, 0x01]);
    }

    /// A clone whose Bluetooth collection declares only Sony's 0x11 still reads the USB offsets:
    /// report 0x11, mask 0x03 at byte 1, the light motor at 4, the heavy one at 5, the colour at
    /// 6..8, and the CRC-32 of the seed 0xA2 and the first 74 bytes (zlib's, computed apart).
    #[test]
    fn a_clone_reads_the_usb_layout_inside_sonys_report() {
        let report = usb_layout_in_bluetooth(
            Motors {
                strong: 200,
                weak: 40,
                ..Motors::STOP
            },
            Some([0x11, 0x9d, 0x2e]),
        );
        assert_eq!(
            &report[..11],
            &[
                0x11, 0x03, 0x00, 0x00, 40, 200, 0x11, 0x9d, 0x2e, 0x00, 0x00
            ]
        );
        assert!(report[11..74].iter().all(|&b| b == 0));
        assert_eq!(report[74..], [0xfd, 0xf6, 0x9a, 0x5e]);
    }

    /// Without a colour the mask drops to the handles alone: no blink bit, so a clone that takes
    /// this report does not flash its lightbar while the game has none picked.
    #[test]
    fn a_clone_report_without_a_light_asks_for_no_blink() {
        let report = usb_layout_in_bluetooth(
            Motors {
                strong: 200,
                weak: 40,
                ..Motors::STOP
            },
            None,
        );
        assert_eq!(&report[..6], &[0x11, 0x01, 0x00, 0x00, 40, 200]);
        assert!(report[6..74].iter().all(|&b| b == 0));
        assert_eq!(report[74..], [0xec, 0x3a, 0x12, 0x38]);
    }

    /// Stopped is zero where the motors are: a release, not a report leaving them on.
    #[test]
    fn stopped_writes_zeroes_where_the_motors_are() {
        let usb = usb(Motors::STOP, None);
        assert_eq!((usb[4], usb[5]), (0, 0));
        let bt = bluetooth(Motors::STOP, None);
        assert_eq!((bt[6], bt[7]), (0, 0));
        let clone = usb_layout_in_bluetooth(Motors::STOP, None);
        assert_eq!((clone[4], clone[5]), (0, 0));
    }
}
