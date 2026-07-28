use super::memory;

pub fn ring_doorbell(doorbell_va: u64, slot_id: u8, target: u8) {
    let db_ptr = (doorbell_va + (slot_id as u64) * 4) as *mut u32;
    unsafe {
        core::ptr::write_volatile(db_ptr, target as u32);
    }
}

pub fn ring_command_doorbell(doorbell_va: u64) {
    ring_doorbell(doorbell_va, 0, 0);
}

/// Wait for a command completion event with a 5 s timeout.
/// Returns `(slot_id, completion_code)` on success.
fn wait_for_completion() -> Result<(u8, u8), &'static str> {
    let mut timeout = crate::platform::x86_64_pc::apic::ApicTimeout::new(5000);
    loop {
        if let Some((slot_id, cc, _param)) = super::event::last_command_completion() {
            if cc == 1 {
                return Ok((slot_id, cc));
            } else {
                return Err("command failed");
            }
        }
        super::event::consume_pending_events();
        if timeout.expired() {
            break;
        }
        core::hint::spin_loop();
    }
    Err("command completion timeout")
}

pub fn submit_enable_slot(
    cmd_ring: &mut memory::TrbRing,
    doorbell_va: u64,
) -> Result<u8, &'static str> {
    let trb = memory::make_enable_slot_trb();
    cmd_ring.enqueue(&trb);
    cmd_ring.flush();
    ring_command_doorbell(doorbell_va);
    let (slot_id, _cc) = wait_for_completion()?;
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
    wait_for_completion()?;
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
    wait_for_completion()?;
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
    wait_for_completion()?;
    Ok(())
}

pub fn submit_no_op(
    cmd_ring: &mut memory::TrbRing,
    doorbell_va: u64,
) -> Result<(), &'static str> {
    let trb = memory::make_no_op_command_trb();
    cmd_ring.enqueue(&trb);
    cmd_ring.flush();
    ring_command_doorbell(doorbell_va);
    wait_for_completion()?;
    Ok(())
}
