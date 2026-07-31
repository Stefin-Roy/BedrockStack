use alloc::vec::Vec;
use crate::drivers::serial::SerialPort;
use super::registers::{PortRegisterSet, PORTSC_CCS, PORTSC_PED, PORTSC_PR, PORTSC_PP, PORTSC_SPEED_MASK, PORTSC_SPEED_SHIFT, PORTSC_STATUS_BITS};

pub struct UsbPort {
    pub port_num: u8,
    pub connected: bool,
    pub enabled: bool,
    pub speed: u8,
    pub resetting: bool,
}

impl UsbPort {
    pub fn new(port_num: u8) -> Self {
        UsbPort {
            port_num,
            connected: false,
            enabled: false,
            speed: 0,
            resetting: false,
        }
    }
}

pub struct UsbPorts {
    pub ports: Vec<UsbPort>,
    pub port_regs: PortRegisterSet,
}

impl UsbPorts {
    pub fn new(max_ports: u8, port_regs: PortRegisterSet) -> Self {
        let mut ports = Vec::new();
        for p in 1..=max_ports {
            ports.push(UsbPort::new(p));
        }
        UsbPorts { ports, port_regs }
    }

    pub fn init_ports(&mut self) -> Result<(), &'static str> {
        let port_count = self.ports.len();
        for i in 0..port_count {
            let port_num = self.ports[i].port_num;
            let portsc = self.port_regs.read_portsc(port_num);
            if portsc & PORTSC_PP == 0 {
                self.port_regs.write_portsc(port_num, portsc | PORTSC_PP);
                // Let the port power rail settle before reading PORTSC again.
                // The earlier read was taken *before* power was applied, so
                // its CCS bit is stale (controllers power ports down at reset).
                crate::services::universal_timer::sleep_ms(20);
            }

            // Fresh read: this is the authoritative power-on state.  Devices
            // still mid link-training raise a PORT_CHANGE event afterwards;
            // detection is event-driven, not a poll loop here.
            let portsc = self.port_regs.read_portsc(port_num);

            if portsc & PORTSC_CCS != 0 {
                let speed = (portsc & PORTSC_SPEED_MASK) >> PORTSC_SPEED_SHIFT;
                self.ports[i].connected = true;
                self.ports[i].speed = speed as u8;

                let speed_str = match speed {
                    1 => "LS", 2 => "FS", 3 => "HS", 4 => "SS", _ => "?",
                };
                SerialPort::puts("[xhci] port ");
                SerialPort::put_u64(port_num as u64);
                SerialPort::puts(": connect, speed=");
                SerialPort::puts(speed_str);
                SerialPort::puts("\n");

                // SuperSpeed ports auto-enable on link training; explicit
                // reset is required only for USB 2.0 (and below) ports.
                if speed != 4 {
                    self.reset_port_by_idx(i)?;
                } else {
                    // SS ports auto-enable on link training
                    let ps = self.port_regs.read_portsc(port_num);
                    if ps & PORTSC_PED != 0 {
                        self.ports[i].enabled = true;
                    }
                }
            }

            let portsc_now = self.port_regs.read_portsc(port_num);
            let status = portsc_now & PORTSC_STATUS_BITS;
            if status != 0 {
                // PORTSC_PED is RW1C – never write 1 back or the port
                // is instantly disabled.  Mask it out here so we only
                // clear the change bits (17..23) without touching PED.
                self.port_regs.write_portsc(port_num, portsc_now & !PORTSC_PED);
            }
        }
        Ok(())
    }

    fn reset_port_by_idx(&mut self, idx: usize) -> Result<(), &'static str> {
        let port_num = self.ports[idx].port_num;
        self.ports[idx].resetting = true;

        let portsc = self.port_regs.read_portsc(port_num);
        // Mask out PED (RW1C) and change bits (RW1C) so the write doesn't
        // accidentally disable the port or clear unhandled status flags.
        self.port_regs.write_portsc(port_num, (portsc & !(PORTSC_PED | PORTSC_STATUS_BITS)) | PORTSC_PR);

        let deadline = crate::services::universal_timer::now_ns() + 500_000_000;
        let pr_cleared = crate::services::universal_timer::wait_until_cond(
            deadline,
            &|| self.port_regs.read_portsc(port_num) & PORTSC_PR == 0,
        );
        if !pr_cleared {
            return Err("port reset timeout");
        }

        let portsc_after = self.port_regs.read_portsc(port_num);

        if portsc_after & PORTSC_PED != 0 {
            self.ports[idx].enabled = true;
            let speed = (portsc_after & PORTSC_SPEED_MASK) >> PORTSC_SPEED_SHIFT;
            self.ports[idx].speed = speed as u8;

            let speed_str = match speed {
                1 => "LS", 2 => "FS", 3 => "HS", 4 => "SS", _ => "?",
            };
            SerialPort::puts("[xhci] port ");
            SerialPort::put_u64(port_num as u64);
            SerialPort::puts(" reset -> enabled, speed=");
            SerialPort::puts(speed_str);
            SerialPort::puts("\n");
        }

        self.ports[idx].resetting = false;
        Ok(())
    }

    fn find_port_idx(&self, port_num: u8) -> Option<usize> {
        self.ports.iter().position(|p| p.port_num == port_num)
    }

    pub fn handle_port_status_change(&mut self, port_num: u8) -> Result<(), &'static str> {
        let idx = self.find_port_idx(port_num).ok_or("invalid port")?;
        let portsc = self.port_regs.read_portsc(port_num);
        let connected = portsc & PORTSC_CCS != 0;
        let enabled = portsc & PORTSC_PED != 0;

        if connected != self.ports[idx].connected {
            if connected {
                let speed = (portsc & PORTSC_SPEED_MASK) >> PORTSC_SPEED_SHIFT;
                self.ports[idx].speed = speed as u8;
                let speed_str = match speed {
                    1 => "LS", 2 => "FS", 3 => "HS", 4 => "SS", _ => "?",
                };
                SerialPort::puts("[xhci] port ");
                SerialPort::put_u64(port_num as u64);
                SerialPort::puts(": connect (");
                SerialPort::puts(speed_str);
                SerialPort::puts(")\n");

                self.reset_port_by_idx(idx)?;
            } else {
                SerialPort::puts("[xhci] port ");
                SerialPort::put_u64(port_num as u64);
                SerialPort::puts(": disconnect\n");
                self.ports[idx].connected = false;
                self.ports[idx].enabled = false;
                self.ports[idx].speed = 0;
            }
            self.ports[idx].connected = connected;
        }

        if !connected && enabled {
            SerialPort::puts("[xhci] port ");
            SerialPort::put_u64(port_num as u64);
            SerialPort::puts(": disabled by disconnect\n");
            self.ports[idx].enabled = false;
        }

        // Re-read PORTSC: reset_port_by_idx() above may have changed PED
        // and set new change bits (PRC).  Using the stale `portsc` would
        // disable the port (write PED=1 back, RW1C) and miss new change
        // bits.
        let portsc_final = self.port_regs.read_portsc(port_num);
        let status_bits = portsc_final & PORTSC_STATUS_BITS;
        if status_bits != 0 {
            self.port_regs.write_portsc(port_num, portsc_final & !PORTSC_PED);
        }

        Ok(())
    }

    pub fn port_speed(&self, port_num: u8) -> Option<u8> {
        self.ports.iter().find(|p| p.port_num == port_num).map(|p| p.speed)
    }

    pub fn reset_port(&mut self, port_num: u8) -> Result<(), &'static str> {
        let idx = self.find_port_idx(port_num).ok_or("invalid port")?;
        self.reset_port_by_idx(idx)
    }

    pub fn port_count(&self) -> u8 {
        self.ports.len() as u8
    }
}
