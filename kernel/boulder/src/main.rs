#![no_std]
#![no_main]

use abyss::allocator::BumpAllocator;
use abyss::frame::BitmapFrameAllocator;
use abyss::memory::MemoryRegionKind;
use abyss::paging::PhysicalAddress;
use abyss::reservation::{Reservation, ReservationKind, ReservationTable};
use boulder::arch::x86_64::{halt, idle};
use boulder::boot::acpi::discover_madt;
use boulder::boot::multiboot2::BootInformation;
use boulder::capability::{
    ArtifactSynthesisControl, Authority, FabricControl, FaultPolicyControl, LearningControl,
    MachineProfileControl, MemorySharingControl, PolicyControl, ResonanceControl,
    UserlandImageControl,
};
use boulder::cpu::topology::{self, ExecutionClass, TopologyPolicy};
use boulder::fabric::{
    Completion, KERNEL_FABRIC, NodeCapabilities, NodeClass, WorkDescriptor, opcode,
};
use boulder::hw::pci;
use boulder::ignition::{BootProtocol, IgnitionSequence};
use boulder::interrupts;
use boulder::mmio::{EARLY_MAPPED_PHYSICAL_LIMIT, direct_map_address, kernel_mmio};
use boulder::serial::SerialPort;
use boulder::shim::{AbyssAllocator, DriverHost, DriverServices, IrqService, MmioService};
use core::alloc::{GlobalAlloc, Layout};
use core::ffi::c_void;
use core::fmt::Write;
use core::panic::PanicInfo;
use core::sync::atomic::{AtomicUsize, Ordering};

core::arch::global_asm!(include_str!("bootstrap.S"), options(att_syntax));

const COM1: u16 = 0x3f8;
const IDENTITY_MAP_END: u64 = 1024 * 1024 * 1024;
const MINIMUM_HEAP_SIZE: u64 = 64 * 1024;
const MAXIMUM_HEAP_SIZE: u64 = 4 * 1024 * 1024;

static KERNEL_HEAP: BumpAllocator = BumpAllocator::empty();
static IRQ_TEST_HITS: AtomicUsize = AtomicUsize::new(0);

unsafe extern "C" {
    static __kernel_start: u8;
    static __kernel_end: u8;
}

unsafe extern "C" fn irq_test_handler(context: *mut c_void) {
    let counter = context.cast::<AtomicUsize>();
    if let Some(counter) = unsafe { counter.as_ref() } {
        counter.fetch_add(1, Ordering::Relaxed);
    }
}

fn map_acpi_region(physical_address: u64, length: usize) -> Option<*const u8> {
    if length == 0
        || physical_address
            .checked_add(length as u64)
            .is_none_or(|end| end > EARLY_MAPPED_PHYSICAL_LIMIT)
    {
        return None;
    }
    direct_map_address(physical_address).map(|address| address as *const u8)
}

#[unsafe(no_mangle)]
pub extern "C" fn boulder_main(multiboot_address: usize) -> ! {
    // SAFETY: The PC-compatible boot environment reserves COM1 for the early
    // kernel console before other drivers are initialized.
    let mut serial = unsafe { SerialPort::initialize(COM1) };
    let _ = writeln!(serial, "Boulder: entering Rust in long mode");
    let mut ignition = IgnitionSequence::new(BootProtocol::Multiboot2);

    // SAFETY: Bootstrap assembly enters with interrupts disabled and installs
    // the GDT selector expected by Boulder's interrupt gates.
    unsafe { interrupts::initialize() };
    interrupts::trigger_breakpoint();
    if interrupts::breakpoint_hits() != 1 {
        let _ = writeln!(serial, "Boulder: breakpoint exception test failed");
        halt();
    }
    let (local_apic, x2apic) = interrupts::apic_capabilities();
    let _ = writeln!(
        serial,
        "Boulder: IDT active, local APIC={}, x2APIC={}",
        local_apic, x2apic
    );

    let kernel_start = core::ptr::addr_of!(__kernel_start) as usize;
    let kernel_end = core::ptr::addr_of!(__kernel_end) as usize;
    let _ = writeln!(serial, "Boulder: kernel {kernel_start:#x}..{kernel_end:#x}");

    // SAFETY: The bootstrap preserves GRUB's Multiboot2 pointer in RDI and
    // identity-maps the first GiB before calling this function.
    let boot = match unsafe { BootInformation::load(multiboot_address) } {
        Ok(boot) => boot,
        Err(error) => {
            let _ = writeln!(serial, "Boulder: invalid boot information: {error:?}");
            halt();
        }
    };
    let _ = writeln!(
        serial,
        "Boulder: Multiboot2 data {:#x}..{:#x}",
        boot.address(),
        boot.address() + boot.total_size()
    );

    let memory_map = match boot.memory_map() {
        Ok(map) => map,
        Err(error) => {
            let _ = writeln!(serial, "Boulder: memory map error: {error:?}");
            halt();
        }
    };
    if let Err(error) = ignition.validate_handoff(memory_map.regions().len()) {
        let _ = writeln!(serial, "Boulder: ignition handoff failed: {error:?}");
        halt();
    }

    let mut usable_bytes = 0_u64;
    for region in memory_map.regions() {
        if region.kind == MemoryRegionKind::Usable {
            usable_bytes = usable_bytes.saturating_add(region.length());
        }
    }
    let _ = writeln!(
        serial,
        "Abyss: accepted {} regions, {} KiB usable",
        memory_map.regions().len(),
        usable_bytes / 1024
    );

    let protected_end = (kernel_end as u64).max((boot.address() + boot.total_size()) as u64);
    let Some(heap_region) =
        memory_map.usable_range(protected_end, IDENTITY_MAP_END, MINIMUM_HEAP_SIZE)
    else {
        let _ = writeln!(serial, "Abyss: no safe bootstrap heap region");
        halt();
    };
    let heap_size = heap_region.length().min(MAXIMUM_HEAP_SIZE) as usize;
    let heap_start = heap_region.start.as_u64() as usize;
    // SAFETY: Abyss selected an identity-mapped usable region above the kernel
    // and boot data. It remains reserved for this allocator after selection.
    if let Err(error) = unsafe { KERNEL_HEAP.initialize(heap_start, heap_size) } {
        let _ = writeln!(serial, "Abyss: heap initialization failed: {error:?}");
        halt();
    }
    let _ = writeln!(
        serial,
        "Abyss: bootstrap heap {heap_start:#x}..{:#x}",
        heap_start + heap_size
    );

    let storage_words = match BitmapFrameAllocator::storage_words(IDENTITY_MAP_END) {
        Ok(words) => words,
        Err(error) => {
            let _ = writeln!(serial, "Abyss: frame bitmap sizing failed: {error:?}");
            halt();
        }
    };
    let storage_layout = match Layout::array::<u64>(storage_words) {
        Ok(layout) => layout,
        Err(_) => {
            let _ = writeln!(serial, "Abyss: invalid frame bitmap layout");
            halt();
        }
    };
    // SAFETY: KERNEL_HEAP is initialized above and the returned allocation is
    // retained exclusively by the frame allocator for the rest of boot.
    let storage_pointer = unsafe { KERNEL_HEAP.alloc(storage_layout) };
    if storage_pointer.is_null() {
        let _ = writeln!(serial, "Abyss: frame bitmap allocation failed");
        halt();
    }
    // SAFETY: The allocation has exactly this many aligned u64 elements and is
    // not accessed through any other reference afterward.
    let storage =
        unsafe { core::slice::from_raw_parts_mut(storage_pointer.cast::<u64>(), storage_words) };

    let mut reservations = ReservationTable::<8>::new();
    let required_reservations = [
        Reservation::new(
            PhysicalAddress::new(0),
            PhysicalAddress::new(0x10_0000),
            ReservationKind::LowMemory,
        ),
        Reservation::new(
            PhysicalAddress::new(kernel_start as u64),
            PhysicalAddress::new(kernel_end as u64),
            ReservationKind::KernelImage,
        ),
        Reservation::new(
            PhysicalAddress::new(boot.address() as u64),
            PhysicalAddress::new((boot.address() + boot.total_size()) as u64),
            ReservationKind::BootInformation,
        ),
        Reservation::new(
            PhysicalAddress::new(heap_start as u64),
            PhysicalAddress::new((heap_start + heap_size) as u64),
            ReservationKind::BootstrapHeap,
        ),
        Reservation::new(
            PhysicalAddress::new(storage_pointer as u64),
            PhysicalAddress::new(storage_pointer as u64 + storage_layout.size() as u64),
            ReservationKind::AllocatorMetadata,
        ),
    ];
    for reservation in required_reservations {
        if let Err(error) = reservations.push(reservation) {
            let _ = writeln!(serial, "Abyss: reservation table failed: {error:?}");
            halt();
        }
    }

    let mut frames = match BitmapFrameAllocator::new(&memory_map, IDENTITY_MAP_END, storage) {
        Ok(allocator) => allocator,
        Err(error) => {
            let _ = writeln!(serial, "Abyss: frame allocator failed: {error:?}");
            halt();
        }
    };
    frames.apply_reservations(&reservations);
    let _ = writeln!(
        serial,
        "Abyss: {} free of {} identity-mapped frames",
        frames.free_frames(),
        frames.managed_frames()
    );
    let Some(test_frame) = frames.allocate() else {
        let _ = writeln!(serial, "Abyss: no frame available for reclaim test");
        halt();
    };
    if let Err(error) = frames.deallocate(test_frame) {
        let _ = writeln!(serial, "Abyss: frame reclaim failed: {error:?}");
        halt();
    }
    let _ = writeln!(
        serial,
        "Abyss: reclaimed test frame at {:#x}",
        test_frame.as_u64()
    );

    let Some(direct_kernel) = direct_map_address(kernel_start as u64) else {
        let _ = writeln!(serial, "Abyss: kernel is outside the direct map");
        halt();
    };
    // SAFETY: Bootstrap assembly maps the same first-GiB physical page at both
    // the identity and higher-half direct-map addresses.
    let direct_map_matches = unsafe {
        (kernel_start as *const u8).read_volatile() == (direct_kernel as *const u8).read_volatile()
    };
    if !direct_map_matches {
        let _ = writeln!(serial, "Abyss: higher-half direct map mismatch");
        halt();
    }
    let _ = writeln!(serial, "Abyss: higher-half direct map verified");
    if let Err(error) = ignition.memory_ready(frames.managed_frames(), frames.free_frames()) {
        let _ = writeln!(serial, "Boulder: ignition memory phase failed: {error:?}");
        halt();
    }

    let rsdp = match boot.rsdp() {
        Ok(rsdp) => rsdp,
        Err(error) => {
            let _ = writeln!(serial, "Boulder: ACPI root pointer error: {error:?}");
            halt();
        }
    };
    // SAFETY: Bootstrap paging keeps the first GiB stable in the direct map,
    // and the mapper rejects every ACPI range outside that mapped window.
    let madt = match unsafe { discover_madt(rsdp, map_acpi_region) } {
        Ok(madt) => madt,
        Err(error) => {
            let _ = writeln!(serial, "Boulder: ACPI MADT discovery failed: {error:?}");
            halt();
        }
    };
    let _ = writeln!(
        serial,
        "Boulder: ACPI revision={} LAPIC={:#x}, I/O APICs={}, overrides={}",
        rsdp.revision,
        madt.local_apic_address,
        madt.io_apics().len(),
        madt.interrupt_source_overrides().len()
    );

    let mmio = kernel_mmio();
    let mapping = match mmio.map(0xb8000, 2, 0) {
        Ok(mapping) => mapping,
        Err(status) => {
            let _ = writeln!(serial, "Boulder: VGA MMIO map failed: {status}");
            halt();
        }
    };
    // SAFETY: The MMIO service returned a live writable mapping for VGA text
    // memory. The mapping remains active through both volatile writes.
    unsafe {
        mapping.pointer.as_ptr().write_volatile(b'S');
        mapping.pointer.as_ptr().add(1).write_volatile(0x0f);
    }
    let _ = writeln!(
        serial,
        "Boulder: MMIO window mapped VGA at {:#x}",
        mapping.pointer.as_ptr() as usize
    );
    let status = mmio.unmap(mapping.handle);
    if status != sisyphus_driver_abi::STATUS_OK {
        let _ = writeln!(serial, "Boulder: VGA MMIO unmap failed: {status}");
        halt();
    }
    if mmio.unmap(mapping.handle) != sisyphus_driver_abi::STATUS_NOT_FOUND {
        let _ = writeln!(serial, "Boulder: stale MMIO handle was accepted");
        halt();
    }
    let _ = writeln!(serial, "Boulder: stale MMIO handle rejected");

    let local_apic = match unsafe { interrupts::initialize_local_apic(mmio) } {
        Ok(info) => info,
        Err(status) => {
            let _ = writeln!(
                serial,
                "Boulder: local APIC initialization failed: {status}"
            );
            halt();
        }
    };
    interrupts::enable();
    let ipi_status = interrupts::send_apic_test_ipi();
    for _ in 0..1_000_000 {
        if interrupts::apic_test_hits() != 0 {
            break;
        }
        core::hint::spin_loop();
    }
    interrupts::disable();
    if ipi_status != sisyphus_driver_abi::STATUS_OK || interrupts::apic_test_hits() != 1 {
        let _ = writeln!(serial, "Boulder: local APIC self-IPI failed: {ipi_status}");
        halt();
    }
    let _ = writeln!(
        serial,
        "Boulder: local APIC id={} version={:#x} at {:#x}, self-IPI verified",
        local_apic.id, local_apic.version, local_apic.physical_address
    );
    if local_apic.physical_address != madt.local_apic_address {
        let _ = writeln!(
            serial,
            "Boulder: local APIC address disagrees with ACPI ({:#x})",
            madt.local_apic_address
        );
        halt();
    }
    let cpu_topology =
        match topology::initialize(&madt, u32::from(local_apic.id), TopologyPolicy::default()) {
            Ok(info) => info,
            Err(error) => {
                let _ = writeln!(serial, "Boulder: CPU topology failed: {error:?}");
                halt();
            }
        };
    if topology::authorize_execution(u32::from(local_apic.id), ExecutionClass::KernelControl)
        .is_err()
    {
        let _ = writeln!(serial, "Boulder: BSP was not assigned the Aegis role");
        halt();
    }
    let _ = writeln!(
        serial,
        "Boulder: CPU topology processors={}, online={}, enclave={}, compute={}",
        cpu_topology.processor_count,
        cpu_topology.online_cores,
        cpu_topology.enclave_cores,
        cpu_topology.compute_cores
    );
    if let Err(error) = ignition.topology_ready(cpu_topology.processor_count) {
        let _ = writeln!(serial, "Boulder: ignition topology phase failed: {error:?}");
        halt();
    }

    // SAFETY: This is the single trusted bootstrap path. Subsystems receive
    // scoped rights from this root instead of constructing authority directly.
    let authority = unsafe { Authority::assume_root() };
    let fabric_control = authority.grant::<FabricControl>();
    let cpu_node = match KERNEL_FABRIC.register_node(
        NodeClass::Cpu,
        0,
        NodeCapabilities::empty(),
        &fabric_control,
    ) {
        Ok(node) => node,
        Err(error) => {
            let _ = writeln!(serial, "Boulder: fabric CPU registration failed: {error:?}");
            halt();
        }
    };
    let fabric_work = match KERNEL_FABRIC.submit(
        WorkDescriptor::new(opcode::NOP, 0, 0, 0),
        NodeClass::Cpu,
        0,
        NodeCapabilities::empty(),
        &fabric_control,
    ) {
        Ok(handle) => handle,
        Err(error) => {
            let _ = writeln!(serial, "Boulder: fabric submission failed: {error:?}");
            halt();
        }
    };
    let taken_work = match KERNEL_FABRIC.take(cpu_node) {
        Ok(Some(work)) => work,
        Ok(None) => {
            let _ = writeln!(serial, "Boulder: fabric CPU queue was unexpectedly empty");
            halt();
        }
        Err(error) => {
            let _ = writeln!(serial, "Boulder: fabric work retrieval failed: {error:?}");
            halt();
        }
    };
    if taken_work.0 != fabric_work || taken_work.1.opcode != opcode::NOP {
        let _ = writeln!(serial, "Boulder: fabric returned the wrong work item");
        halt();
    }
    if let Err(error) = KERNEL_FABRIC.complete(fabric_work, Ok(())) {
        let _ = writeln!(serial, "Boulder: fabric completion failed: {error:?}");
        halt();
    }
    if KERNEL_FABRIC.completion(fabric_work) != Ok(Completion::Succeeded) {
        let _ = writeln!(serial, "Boulder: fabric completion state was not published");
        halt();
    }
    if let Err(error) = KERNEL_FABRIC.release(fabric_work, &fabric_control) {
        let _ = writeln!(serial, "Boulder: fabric release failed: {error:?}");
        halt();
    }
    let _ = writeln!(
        serial,
        "Boulder: capability-gated fabric work cycle verified"
    );

    let policy_control = authority.grant::<PolicyControl>();
    if let Err(error) = boulder::aether::initialize(&policy_control) {
        let _ = writeln!(serial, "Boulder: Aether initialization failed: {error:?}");
        halt();
    }
    if boulder::aether::policy_allows_page_count(512) != Ok(true)
        || boulder::aether::policy_allows_page_count(513) != Ok(false)
        || boulder::aether::recorded_events() < 3
    {
        let _ = writeln!(serial, "Boulder: Aether policy or recorder test failed");
        halt();
    }
    let _ = writeln!(
        serial,
        "Boulder: Aether policy and bounded flight recorder verified"
    );

    let resonance_control = authority.grant::<ResonanceControl>();
    let learning_control = authority.grant::<LearningControl>();
    let memory_sharing = authority.grant::<MemorySharingControl>();
    let fault_policy = authority.grant::<FaultPolicyControl>();
    let artifact_synthesis = authority.grant::<ArtifactSynthesisControl>();
    let userland_image = authority.grant::<UserlandImageControl>();
    let blacklab = match boulder::blacklab::initialize(
        &resonance_control,
        &learning_control,
        &memory_sharing,
        &fault_policy,
        &artifact_synthesis,
        &userland_image,
    ) {
        Ok(summary) => summary,
        Err(error) => {
            let _ = writeln!(
                serial,
                "Boulder: Black Lab initialization failed: {error:?}"
            );
            halt();
        }
    };
    let _ = writeln!(
        serial,
        "Boulder: Black Lab time={} ns, heat={}, predictions={}, epoch={}, generation={}, faults={}, artifact={} bytes, PID1 plan entry={:#x}",
        blacklab.logical_nanoseconds,
        blacklab.semantic_heat,
        blacklab.predictions,
        blacklab.next_epoch,
        blacklab.evolution_generation,
        blacklab.quarantined_faults,
        blacklab.materialized_bytes,
        blacklab.pid1_entry_point
    );

    // SAFETY: ACPI described the active controllers, the local APIC is live,
    // and interrupts are disabled after the completed self-IPI test.
    let io_apics = match unsafe { interrupts::initialize_io_apics(&madt, mmio, local_apic.id) } {
        Ok(info) => info,
        Err(error) => {
            let _ = writeln!(serial, "Boulder: I/O APIC initialization failed: {error:?}");
            halt();
        }
    };
    let _ = writeln!(
        serial,
        "Boulder: {} I/O APIC(s), {} redirection entries, {} source override(s)",
        io_apics.controller_count,
        io_apics.redirection_entries,
        io_apics.interrupt_source_overrides
    );

    // SAFETY: The x86 PC boot platform exposes PCI configuration mechanism
    // one, and no driver can access its ports before this early inventory.
    let pci_inventory = unsafe { pci::scan_buses() };
    if pci_inventory.overflowed() {
        let _ = writeln!(serial, "Boulder: PCI inventory capacity exceeded");
        halt();
    }
    let _ = writeln!(
        serial,
        "Boulder: discovered {} PCI function(s)",
        pci_inventory.devices().len()
    );

    let machine_profile_control = authority.grant::<MachineProfileControl>();
    let kairos = match boulder::kairos::initialize(
        &madt,
        &memory_map,
        &pci_inventory,
        &machine_profile_control,
    ) {
        Ok(summary) => summary,
        Err(error) => {
            let _ = writeln!(serial, "Boulder: Kairos initialization failed: {error:?}");
            halt();
        }
    };
    let _ = writeln!(
        serial,
        "Boulder: Kairos profile CPUs={}, memory={}, I/O={}, domains={}",
        kairos.processors, kairos.memory_regions, kairos.io_devices, kairos.domains
    );
    if let Err(error) = ignition.subsystems_ready() {
        let _ = writeln!(
            serial,
            "Boulder: ignition subsystem phase failed: {error:?}"
        );
        halt();
    }

    // SAFETY: Interrupts remain disabled and PIT channel 2 has not been
    // assigned to another subsystem during early boot.
    let timer = match unsafe { interrupts::initialize_local_apic_timer(10) } {
        Ok(timer) => timer,
        Err(error) => {
            let _ = writeln!(serial, "Boulder: local APIC timer failed: {error:?}");
            halt();
        }
    };
    interrupts::enable();
    for _ in 0..100_000_000 {
        if interrupts::apic_timer_hits() >= 2 {
            break;
        }
        core::hint::spin_loop();
    }
    interrupts::disable();
    if interrupts::apic_timer_hits() < 2 {
        let _ = writeln!(serial, "Boulder: local APIC timer delivery timed out");
        halt();
    }
    let _ = writeln!(
        serial,
        "Boulder: local APIC timer {} Hz, {} ms period verified",
        timer.ticks_per_second, timer.period_milliseconds
    );

    let driver_allocator = AbyssAllocator::new(&KERNEL_HEAP);
    let irq = interrupts::kernel_irq();
    IRQ_TEST_HITS.store(0, Ordering::Relaxed);
    let irq_handle = match irq.register(
        5,
        0,
        irq_test_handler,
        core::ptr::addr_of!(IRQ_TEST_HITS) as *mut c_void,
    ) {
        Ok(handle) => handle,
        Err(status) => {
            let _ = writeln!(serial, "Boulder: IRQ registration failed: {status}");
            halt();
        }
    };
    if irq.set_enabled(irq_handle, true) != sisyphus_driver_abi::STATUS_OK {
        let _ = writeln!(serial, "Boulder: IRQ enable failed");
        halt();
    }
    unsafe { core::arch::asm!("int 0x25", options(nomem, nostack)) };
    if IRQ_TEST_HITS.load(Ordering::Relaxed) != 1 {
        let _ = writeln!(serial, "Boulder: IRQ gate test failed");
        halt();
    }
    if irq.unregister(irq_handle) != sisyphus_driver_abi::STATUS_OK {
        let _ = writeln!(serial, "Boulder: IRQ unregister failed");
        halt();
    }
    if irq.set_enabled(irq_handle, true) != sisyphus_driver_abi::STATUS_NOT_FOUND {
        let _ = writeln!(serial, "Boulder: stale IRQ handle was accepted");
        halt();
    }
    let _ = writeln!(serial, "Boulder: IRQ 5 gate and stale handle verified");

    let driver_services = DriverServices::new()
        .with_allocator(&driver_allocator)
        .with_mmio(mmio)
        .with_irq(irq);
    let driver_host = DriverHost::new(&driver_services);
    let _ = writeln!(
        serial,
        "Boulder: driver memory capabilities {:#x}",
        driver_host.api().capabilities
    );
    if let Err(error) = ignition.interrupts_ready() {
        let _ = writeln!(
            serial,
            "Boulder: ignition interrupt phase failed: {error:?}"
        );
        halt();
    }
    interrupts::enable();
    let ignition_summary = match ignition.online() {
        Ok(summary) => summary,
        Err(error) => {
            let _ = writeln!(serial, "Boulder: ignition online phase failed: {error:?}");
            halt();
        }
    };
    let _ = writeln!(
        serial,
        "Boulder: ignition {:?} online, userland_ready={}",
        ignition_summary.protocol, ignition_summary.userland_ready
    );
    let _ = writeln!(serial, "Boulder: interrupt-routing milestone complete");

    idle()
}

#[panic_handler]
fn panic(info: &PanicInfo<'_>) -> ! {
    // SAFETY: Panic handling occurs after the boot environment has made COM1
    // available, and no recovery path returns from this handler.
    let mut serial = unsafe { SerialPort::initialize(COM1) };
    let _ = writeln!(serial, "Boulder panic: {info}");
    halt()
}
