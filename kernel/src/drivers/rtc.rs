//! x86_64 CMOS RTC driver.
//!
//! Reads the battery-backed wall clock through the MC146818 RTC ports
//! (0x70 index / 0x71 data). QEMU's PIIX3/ICH9 RTC mirrors the host clock.
//! Returns seconds since the Unix epoch.

use crate::platform::x86_64_pc::pit::{inb, outb};

const CMOS_INDEX: u16 = 0x70;
const CMOS_DATA: u16 = 0x71;

const REG_SECONDS: u8 = 0x00;
const REG_MINUTES: u8 = 0x02;
const REG_HOURS: u8 = 0x04;
const REG_DAY: u8 = 0x07;
const REG_MONTH: u8 = 0x08;
const REG_YEAR: u8 = 0x09;
const REG_CENTURY: u8 = 0x32;
const REG_STATUS_A: u8 = 0x0A;
const REG_STATUS_B: u8 = 0x0B;

/// Update-in-progress flag in Status Register A.
const UIP: u8 = 0x80;
/// Status Register B: binary vs BCD data.
const BINARY: u8 = 0x04;
/// Status Register B: 24-hour vs 12-hour mode.
const HOUR_12: u8 = 0x02;

fn read_cmos(reg: u8) -> u8 {
    // Bit 7 of the index port disables NMI during the access.
    outb(CMOS_INDEX, reg & 0x7F);
    inb(CMOS_DATA)
}

fn bcd_to_binary(bcd: u8) -> u8 {
    (bcd & 0x0F) + (bcd >> 4) * 10
}

struct RtcTime {
    sec: u8,
    min: u8,
    hour: u8,
    day: u8,
    month: u8,
    year: u16,
}

/// Read all RTC fields once.  Must only be called when UIP is clear, otherwise
/// the values may straddle an update and be inconsistent.
fn read_all() -> RtcTime {
    let status_b = read_cmos(REG_STATUS_B);
    let binary = status_b & BINARY != 0;
    let hour12 = status_b & HOUR_12 == 0;

    let mut sec = read_cmos(REG_SECONDS);
    let mut min = read_cmos(REG_MINUTES);
    let mut hour = read_cmos(REG_HOURS);
    let mut day = read_cmos(REG_DAY);
    let mut month = read_cmos(REG_MONTH);
    let mut year = read_cmos(REG_YEAR);
    let century = read_cmos(REG_CENTURY);

    if !binary {
        sec = bcd_to_binary(sec);
        min = bcd_to_binary(min);
        hour = bcd_to_binary(hour & 0x7F);
        day = bcd_to_binary(day);
        month = bcd_to_binary(month);
        year = bcd_to_binary(year);
    }

    if hour12 {
        // Bit 7 of the hour register is the PM flag in 12-hour mode.
        let pm = read_cmos(REG_HOURS) & 0x80 != 0;
        if pm && hour != 12 {
            hour += 12;
        } else if !pm && hour == 12 {
            hour = 0;
        }
    }

    RtcTime {
        sec,
        min,
        hour,
        day,
        month,
        year: (century as u16) * 100 + year as u16,
    }
}

pub fn read_epoch_secs() -> Option<u64> {
    // Wait for an RTC update to complete so the read is consistent.
    for _ in 0..200 {
        if read_cmos(REG_STATUS_A) & UIP == 0 {
            break;
        }
        crate::services::universal_timer::sleep_ms(1);
    }
    if read_cmos(REG_STATUS_A) & UIP != 0 {
        return None;
    }

    let t = read_all();
    let year = t.year as u64;
    let month = t.month as u64;
    let day = t.day as u64;
    if year < 1970 || year > 9999 || month == 0 || month > 12 || day == 0 || day > 31 {
        return None;
    }

    let days = crate::services::wallclock::days_from_civil(year, month, day);
    Some(
        days * 86400
            + (t.hour as u64) * 3600
            + (t.min as u64) * 60
            + t.sec as u64,
    )
}
