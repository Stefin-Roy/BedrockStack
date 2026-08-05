use super::memory;
use crate::drivers::serial::SerialPort;

pub fn ring_doorbell(doorbell_va: u64, slot_id: u8, target: u8) {
    let db_ptr = (doorbell_va + (slot_id as u64) * 4) as *mut u32;
    unsafe {
        core::ptr::write_volatile(db_ptr, target as u32);
    }
}

pub fn ring_command_doorbell(doorbell_va: u64) {
    ring_doorbell(doorbell_va, 0, 0);
}

/// Wait for a command completion event with a 5 s timeout.
/// Returns `(slot_id, completion_code)` on success.
///
/// `expected_slot` is the slot the command was issued for; pass `0` when the
/// completion carries the slot assignment itself (Enable Slot).  A completion
/// for a different slot is a stale/foreign event and is never trusted.
fn wait_for_completion(expected_slot: u8) -> Result<(u8, u8), &'static str> {
    use crate::services::universal_timer::{now_ns, wait_until_cond};
    let deadline = now_ns() + 5_000_000_000;
    let completed = wait_until_cond(deadline, &|| {
        super::event::consume_pending_events();
        super::event::peek_last_command_completion().is_some()
    });
    if !completed {
        SerialPort::puts("[xhci] CMD TIMEOUT\n");
        return Err("command completion timeout");
    }
    let (slot_id, cc, _param) = match super::event::last_command_completion() {
        Some(c) => c,
        None => {
            SerialPort::puts("[xhci] CMD completion lost\n");
            return Err("command completion lost");
        }
    };
    if expected_slot != 0 && slot_id != expected_slot {
        SerialPort::puts("[xhci] CMD stale completion slot=");
        SerialPort::put_u64(slot_id as u64);
        SerialPort::puts(" expected=");
        SerialPort::put_u64(expected_slot as u64);
        SerialPort::puts("\n");
        return Err("stale completion");
    }
    if cc == 1 {
        Ok((slot_id, cc))
    } else {
        SerialPort::puts("[xhci] CMD FAIL cc=");
        SerialPort::put_u64(cc as u64);
        SerialPort::puts(" slot=");
        SerialPort::put_u64(slot_id as u64);
        SerialPort::puts("\n");
        Err("command failed")
    }
}

pub fn submit_enable_slot(
    cmd_ring: &mut memory::TrbRing,
    doorbell_va: u64,
) -> Result<u8, &'static str> {
    let trb = memory::make_enable_slot_trb();
    cmd_ring.enqueue(&trb);
    cmd_ring.flush();
    ring_command_doorbell(doorbell_va);
    let (slot_id, _cc) = wait_for_completion(0)?;
    Ok(slot_id)
}

pub fn submit_address_device(
    cmd_ring: &mut memory::TrbRing,
    doorbell_va: u64,
    input_ctx_phys: u64,
    slot_id: u8,
    bsr: bool,
) -> Result<(), &'static str> {
    let trb = memory::make_address_device_trb(input_ctx_phys, slot_id, bsr);
    cmd_ring.enqueue(&trb);
    cmd_ring.flush();
    ring_command_doorbell(doorbell_va);
    wait_for_completion(slot_id)?;
    Ok(())
}

pub fn submit_configure_endpoint(
    cmd_ring: &mut memory::TrbRing,
    doorbell_va: u64,
    input_ctx_phys: u64,
    slot_id: u8,
    deconfigure: bool,
) -> Result<(), &'static str> {
    let trb = memory::make_configure_endpoint_trb(input_ctx_phys, slot_id, deconfigure);
    cmd_ring.enqueue(&trb);
    cmd_ring.flush();
    ring_command_doorbell(doorbell_va);
    wait_for_completion(slot_id)?;
    Ok(())
}

pub fn submit_evaluate_context(
    cmd_ring: &mut memory::TrbRing,
    doorbell_va: u64,
    ctx_phys: u64,
    slot_id: u8,
) -> Result<(), &'static str> {
    let trb = memory::make_evaluate_context_trb(ctx_phys, slot_id);
    cmd_ring.enqueue(&trb);
    cmd_ring.flush();
    ring_command_doorbell(doorbell_va);
    wait_for_completion(slot_id)?;
    Ok(())
}

pub fn submit_disable_slot(
    cmd_ring: &mut memory::TrbRing,
    doorbell_va: u64,
    slot_id: u8,
) -> Result<(), &'static str> {
    let trb = memory::make_disable_slot_trb(slot_id);
    cmd_ring.enqueue(&trb);
    cmd_ring.flush();
    ring_command_doorbell(doorbell_va);
    wait_for_completion(slot_id)?;
    Ok(())
}
