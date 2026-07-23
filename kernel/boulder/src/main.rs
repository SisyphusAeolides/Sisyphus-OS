#![no_std]
#![no_main]

use abyss::allocator::BumpAllocator;
use abyss::frame::BitmapFrameAllocator;
use abyss::memory::MemoryRegionKind;
use abyss::paging::PhysicalAddress;
use abyss::reservation::{Reservation, ReservationKind, ReservationTable};
use boulder::arch::x86_64::{active_page_table_root, enable_execute_disable, halt, privilege};
use boulder::boot::acpi::discover_madt;
use boulder::boot::multiboot2::BootInformation;
use boulder::capability::{
    ArtifactSynthesisControl, Authority, FabricControl, FaultPolicyControl, LearningControl,
    MachineProfileControl, MemorySharingControl, PhysicalMemoryControl, PolicyControl,
    ProcessInstallControl, ResonanceControl, UserlandImageControl,
};
use boulder::cpu::topology::{self, ExecutionClass, TopologyPolicy};
use boulder::fabric::{
    Completion, KERNEL_FABRIC, NodeCapabilities, NodeClass, WorkDescriptor, opcode,
};
use boulder::hw::pci;
use boulder::ignition::{BootProtocol, IgnitionSequence};
use boulder::interrupts;
use boulder::mmio::{
    EARLY_MAPPED_PHYSICAL_LIMIT, HIGHER_HALF_DIRECT_MAP_BASE, KERNEL_VIRTUAL_BASE,
    direct_map_address, kernel_mmio,
};
use boulder::process::install::UserAddressSpaceBackend;
use boulder::process::x86_64::{
    DirectMapFrameMemory, FrameBackedAddressSpace, INITIAL_USER_STACK_PAGES,
};
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
const KERNEL_PHYSICAL_LOAD_BASE: u64 = 1024 * 1024;
const MINIMUM_HEAP_SIZE: u64 = 64 * 1024;
const MAXIMUM_HEAP_SIZE: u64 = 4 * 1024 * 1024;

#[cfg(target_os = "none")]
const PUSH_EXPECTED_SHA256: [u8; 32] = parse_sha256(env!("SISYPHUS_PUSH_SHA256"));
#[cfg(not(target_os = "none"))]
const PUSH_EXPECTED_SHA256: [u8; 32] = [0; 32];
#[cfg(target_os = "none")]
const PUSH_EXPECTED_BYTES: usize = parse_decimal(env!("SISYPHUS_PUSH_BYTES"));
#[cfg(not(target_os = "none"))]
const PUSH_EXPECTED_BYTES: usize = 0;
#[cfg(target_os = "none")]
const PUSH_ENTRY_FILE_OFFSET: usize = parse_decimal(env!("SISYPHUS_PUSH_ENTRY_FILE_OFFSET"));
#[cfg(not(target_os = "none"))]
const PUSH_ENTRY_FILE_OFFSET: usize = 0;

#[global_allocator]
static KERNEL_HEAP: BumpAllocator = BumpAllocator::empty();
static IRQ_TEST_HITS: AtomicUsize = AtomicUsize::new(0);

const fn parse_sha256(encoded: &str) -> [u8; 32] {
    assert!(encoded.len() == 64, "invalid embedded Push digest");
    let bytes = encoded.as_bytes();
    let mut digest = [0_u8; 32];
    let mut index = 0;
    while index < digest.len() {
        digest[index] = (hex_nibble(bytes[index * 2]) << 4) | hex_nibble(bytes[index * 2 + 1]);
        index += 1;
    }
    digest
}

const fn hex_nibble(value: u8) -> u8 {
    match value {
        b'0'..=b'9' => value - b'0',
        b'a'..=b'f' => value - b'a' + 10,
        _ => panic!("invalid embedded Push digest"),
    }
}

const fn parse_decimal(encoded: &str) -> usize {
    assert!(!encoded.is_empty(), "invalid embedded Push size");
    let bytes = encoded.as_bytes();
    let mut value = 0_usize;
    let mut index = 0;
    while index < bytes.len() {
        assert!(bytes[index].is_ascii_digit(), "invalid embedded Push size");
        value = match value.checked_mul(10) {
            Some(value) => value,
            None => panic!("embedded Push size overflow"),
        };
        value = match value.checked_add((bytes[index] - b'0') as usize) {
            Some(value) => value,
            None => panic!("embedded Push size overflow"),
        };
        index += 1;
    }
    value
}

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
pub extern "C" fn boulder_main(multiboot_address: usize, multiboot_physical_address: usize) -> ! {
    // SAFETY: The PC-compatible boot environment reserves COM1 for the early
    // kernel console before other drivers are initialized.
    let mut serial = unsafe { SerialPort::initialize(COM1) };
    let _ = writeln!(serial, "Boulder: entering Rust in long mode");
    // SAFETY: Serialized BSP bootstrap owns CR3, and the bootstrap direct map
    // covers the physical root frame used for this read-only transition gate.
    let bootstrap_root = unsafe { active_page_table_root() };
    let Some(bootstrap_root_virtual) = direct_map_address(bootstrap_root) else {
        let _ = writeln!(
            serial,
            "Boulder: bootstrap page-table root is outside the direct map"
        );
        halt();
    };
    // SAFETY: The active root frame is mapped for inspection through the
    // stable higher-half direct map during serialized bootstrap.
    let low_pml4_entry = unsafe { (bootstrap_root_virtual as *const u64).read_volatile() };
    let stack_address = core::ptr::addr_of!(serial) as usize;
    if low_pml4_entry != 0
        || (boulder_main as *const () as usize) < KERNEL_VIRTUAL_BASE
        || stack_address < KERNEL_VIRTUAL_BASE
    {
        let _ = writeln!(
            serial,
            "Boulder: higher-half transition gate failed: low={low_pml4_entry:#x}, code={:#x}, stack={stack_address:#x}",
            boulder_main as *const () as usize,
        );
        halt();
    }
    let _ = writeln!(
        serial,
        "Boulder: higher-half transition verified, low PML4 entry absent"
    );
    // SAFETY: BSP bootstrap is serialized with interrupts disabled, and every
    // process root inherits the higher-half descriptor and RSP0 storage.
    let privilege_info = match unsafe { privilege::initialize() } {
        Ok(info) => info,
        Err(error) => {
            let _ = writeln!(serial, "Boulder: privilege tables failed: {error:?}");
            halt();
        }
    };
    let _ = writeln!(
        serial,
        "Boulder: TSS active RSP0={:#x}, user selectors={:#x}/{:#x}",
        privilege_info.kernel_stack_top,
        privilege_info.user_code_selector,
        privilege_info.user_data_selector,
    );
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
    let Some(kernel_physical_start) = kernel_start.checked_sub(KERNEL_VIRTUAL_BASE) else {
        let _ = writeln!(serial, "Boulder: kernel start is outside the higher half");
        halt();
    };
    let Some(kernel_physical_end) = kernel_end.checked_sub(KERNEL_VIRTUAL_BASE) else {
        let _ = writeln!(serial, "Boulder: kernel end is outside the higher half");
        halt();
    };
    let _ = writeln!(
        serial,
        "Boulder: kernel virtual {kernel_start:#x}..{kernel_end:#x}, physical {kernel_physical_start:#x}..{kernel_physical_end:#x}"
    );

    // SAFETY: The bootstrap preserves GRUB's physical Multiboot2 pointer and
    // passes its mapped higher-half direct-map alias in the first argument.
    let boot = match unsafe { BootInformation::load(multiboot_address) } {
        Ok(boot) => boot,
        Err(error) => {
            let _ = writeln!(serial, "Boulder: invalid boot information: {error:?}");
            halt();
        }
    };
    let _ = writeln!(
        serial,
        "Boulder: Multiboot2 physical data {:#x}..{:#x}",
        multiboot_physical_address,
        multiboot_physical_address + boot.total_size()
    );
    let push_module = match boot.module(b"push") {
        Ok(module) => module,
        Err(error) => {
            let _ = writeln!(serial, "Boulder: Push boot module error: {error:?}");
            halt();
        }
    };
    if push_module.length() as usize != PUSH_EXPECTED_BYTES
        || push_module.end.as_u64() > EARLY_MAPPED_PHYSICAL_LIMIT
    {
        let _ = writeln!(serial, "Boulder: Push boot module size or range mismatch");
        halt();
    }
    let Some(push_virtual) = direct_map_address(push_module.start.as_u64()) else {
        let _ = writeln!(
            serial,
            "Boulder: Push boot module is outside the direct map"
        );
        halt();
    };
    // SAFETY: The validated module range is immutable bootloader-owned memory
    // covered by the retained direct map and reserved below before allocation.
    let push_bytes = unsafe {
        core::slice::from_raw_parts(push_virtual as *const u8, push_module.length() as usize)
    };
    let _ = writeln!(
        serial,
        "Boulder: measured Push module {} bytes at {:#x}..{:#x}",
        push_bytes.len(),
        push_module.start.as_u64(),
        push_module.end.as_u64(),
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

    let protected_end = (kernel_physical_end as u64)
        .max((multiboot_physical_address + boot.total_size()) as u64)
        .max(push_module.end.as_u64());
    let Some(heap_region) =
        memory_map.usable_range(protected_end, IDENTITY_MAP_END, MINIMUM_HEAP_SIZE)
    else {
        let _ = writeln!(serial, "Abyss: no safe bootstrap heap region");
        halt();
    };
    let heap_size = heap_region.length().min(MAXIMUM_HEAP_SIZE) as usize;
    let heap_start = heap_region.start.as_u64() as usize;
    let Some(heap_virtual_start) = direct_map_address(heap_start as u64) else {
        let _ = writeln!(serial, "Abyss: bootstrap heap is outside the direct map");
        halt();
    };
    // SAFETY: Abyss selected an identity-mapped usable region above the kernel
    // and boot data. It remains reserved for this allocator after selection.
    if let Err(error) = unsafe { KERNEL_HEAP.initialize(heap_virtual_start, heap_size) } {
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
    let Some(storage_physical) =
        (storage_pointer as usize).checked_sub(HIGHER_HALF_DIRECT_MAP_BASE)
    else {
        let _ = writeln!(serial, "Abyss: frame bitmap is outside the direct map");
        halt();
    };
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
            PhysicalAddress::new(KERNEL_PHYSICAL_LOAD_BASE),
            PhysicalAddress::new(kernel_physical_end as u64),
            ReservationKind::KernelImage,
        ),
        Reservation::new(
            PhysicalAddress::new(multiboot_physical_address as u64),
            PhysicalAddress::new((multiboot_physical_address + boot.total_size()) as u64),
            ReservationKind::BootInformation,
        ),
        Reservation::new(
            push_module.start,
            push_module.end,
            ReservationKind::BootModule,
        ),
        Reservation::new(
            PhysicalAddress::new(heap_start as u64),
            PhysicalAddress::new((heap_start + heap_size) as u64),
            ReservationKind::BootstrapHeap,
        ),
        Reservation::new(
            PhysicalAddress::new(storage_physical as u64),
            PhysicalAddress::new(storage_physical as u64 + storage_layout.size() as u64),
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

    let Some(direct_kernel) = direct_map_address(kernel_physical_start as u64) else {
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
    let process_install = authority.grant::<ProcessInstallControl>();
    let physical_memory = authority.grant::<PhysicalMemoryControl>();
    // SAFETY: Bootstrap is serialized at ring 0 and no process page tables
    // containing NX entries can be activated before this feature gate.
    if let Err(error) = unsafe { enable_execute_disable() } {
        let _ = writeln!(serial, "Boulder: execute-disable unavailable: {error:?}");
        halt();
    }
    // SAFETY: CR3 is read during serialized BSP bootstrap and only used as an
    // immutable source for the kernel half of a new, inactive hierarchy.
    let kernel_page_table_root = PhysicalAddress::new(unsafe { active_page_table_root() });
    // SAFETY: The bitmap allocator manages only the first GiB, which the
    // bootstrap maps at this stable writable higher-half direct-map base.
    let frame_memory = unsafe {
        DirectMapFrameMemory::new(
            &mut frames,
            HIGHER_HALF_DIRECT_MAP_BASE,
            EARLY_MAPPED_PHYSICAL_LIMIT,
            &physical_memory,
        )
    };
    let mut process_backend =
        FrameBackedAddressSpace::new(frame_memory, kernel_page_table_root, &process_install);
    let controls = boulder::blacklab::Controls {
        resonance: &resonance_control,
        learning: &learning_control,
        memory_sharing: &memory_sharing,
        fault_policy: &fault_policy,
        artifact_synthesis: &artifact_synthesis,
        userland_image: &userland_image,
        process_install: &process_install,
    };
    let initialized = match boulder::blacklab::initialize(
        controls,
        &mut process_backend,
        boulder::blacklab::Pid1Source {
            bytes: push_bytes,
            expected_sha256: PUSH_EXPECTED_SHA256,
            entry_file_offset: PUSH_ENTRY_FILE_OFFSET,
        },
    ) {
        Ok(initialized) => initialized,
        Err(error) => {
            let _ = writeln!(
                serial,
                "Boulder: Black Lab initialization failed: {error:?}"
            );
            halt();
        }
    };
    let blacklab = initialized.summary;
    let pid1 = initialized.pid1;
    if blacklab.pid1_page_table_root.is_none()
        || blacklab.pid1_owned_frames == 0
        || !blacklab.pid1_activation_validated
        || process_backend.owned_frame_count() != blacklab.pid1_owned_frames
    {
        let _ = writeln!(serial, "Boulder: PID1 retained ownership failed");
        halt();
    }
    let _stack_top = match process_backend.install_initial_stack(&pid1, &process_install) {
        Ok(stack) => stack,
        Err(error) => {
            let _ = writeln!(serial, "Boulder: PID1 stack installation failed: {error:?}");
            halt();
        }
    };
    if let Err(error) = process_backend.install_thermal_page(&pid1, &process_install) {
        let _ = writeln!(
            serial,
            "Boulder: PID1 thermal page mapping failed: {error:?}"
        );
        halt();
    }

    let _root_token = boulder::nexus_runtime::initialize(&resonance_control).unwrap();
    boulder::nexus_plane::initialize(&learning_control).unwrap();

    if let Err(error) = process_backend.install_nexus_plane(&pid1, &process_install) {
        let _ = writeln!(
            serial,
            "Boulder: PID1 nexus plane mapping failed: {error:?}"
        );
        halt();
    }

    let pid1_stack = match process_backend.prepare_initial_stack(
        &pid1,
        &[b"push"],
        &[b"SISYPHUS_PROCESS=push", b"SISYPHUS_ABI=1"],
    ) {
        Ok(stack) => stack,
        Err(error) => {
            let _ = writeln!(
                serial,
                "Boulder: PID1 argv/envp preparation failed: {error:?}"
            );
            halt();
        }
    };
    let Some(pid1_info) = process_backend.process_info(&pid1) else {
        let _ = writeln!(serial, "Boulder: retained PID1 handle became stale");
        halt();
    };
    let Some(pid1_root) = pid1_info.address_space_root else {
        let _ = writeln!(serial, "Boulder: retained PID1 has no page-table root");
        halt();
    };
    if pid1_info.initial_stack_pointer != Some(pid1_stack)
        || pid1_info.owned_frames < blacklab.pid1_owned_frames + INITIAL_USER_STACK_PAGES
    {
        let _ = writeln!(
            serial,
            "Boulder: retained PID1 stack metadata is inconsistent"
        );
        halt();
    }
    let _ = writeln!(
        serial,
        "Boulder: Black Lab time={} ns, heat={}, predictions={}, epoch={}, generation={}, faults={}, artifact={} bytes, PID1 plan entry={:#x}, install=frame-backed:{}",
        blacklab.logical_nanoseconds,
        blacklab.semantic_heat,
        blacklab.predictions,
        blacklab.next_epoch,
        blacklab.evolution_generation,
        blacklab.quarantined_faults,
        blacklab.materialized_bytes,
        blacklab.pid1_entry_point,
        blacklab.pid1_install_generation
    );
    let _ = writeln!(
        serial,
        "Boulder: PID1 page-table root={:#x}, frames={}, segments={}, retained=true, cr3_activation=validated, argv_envp=prepared, launch=pending",
        pid1_root, pid1_info.owned_frames, pid1_info.segment_count,
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
    if let Err(error) = ignition.userland_ready() {
        let _ = writeln!(serial, "Boulder: ignition userland phase failed: {error:?}");
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
    let _ = writeln!(
        serial,
        "Boulder: transferring permanently to measured Push PID1 at {:#x}",
        pid1_info.entry_point,
    );
    interrupts::disable();
    // SAFETY: Push's measured W^X image, retained hierarchy, and RW+NX stack
    // remain owned by `process_backend`, all kernel entry mappings are
    // inherited, and this terminal transfer intentionally abandons the
    // bootstrap stack without running destructors.
    if let Err(error) = unsafe {
        privilege::enter_user_process(
            pid1_info.entry_point as usize,
            pid1_stack as usize,
            pid1_root,
        )
    } {
        let _ = writeln!(
            serial,
            "Boulder: persistent PID1 transfer failed: {error:?}"
        );
    }
    halt()
}

#[panic_handler]
fn panic(info: &PanicInfo<'_>) -> ! {
    // SAFETY: Panic handling occurs after the boot environment has made COM1
    // available, and no recovery path returns from this handler.
    let mut serial = unsafe { SerialPort::initialize(COM1) };
    let _ = writeln!(serial, "Boulder panic: {info}");
    halt()
}
