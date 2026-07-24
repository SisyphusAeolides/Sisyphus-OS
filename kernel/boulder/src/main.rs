#![no_std]
#![no_main]

use abyss::allocator::BumpAllocator;
use abyss::frame::BitmapFrameAllocator;
use abyss::memory::MemoryRegionKind;
use abyss::paging::PhysicalAddress;
use abyss::reservation::{Reservation, ReservationKind, ReservationTable};
use boulder::arch::x86_64::{active_page_table_root, enable_execute_disable, halt, privilege};
use boulder::boot::acpi::{discover_dmar, discover_madt};
use boulder::boot::multiboot2::BootInformation;
use boulder::capability::{
    ArtifactSynthesisControl, Authority, DeviceMemoryControl, FabricControl, FaultPolicyControl,
    LearningControl, MachineProfileControl, MemorySharingControl, PciConfigurationControl,
    PhysicalMemoryControl, PolicyControl, ProcessInstallControl, ResonanceControl,
    UserlandImageControl,
};
use boulder::cpu::topology::{self, ExecutionClass, TopologyPolicy};
use boulder::drivers::device_census::{
    AUTHORITY_CLOCK, AUTHORITY_DELEGATE, AUTHORITY_DMA, AUTHORITY_MMIO, AUTHORITY_PCI_CONFIG,
    BootDeviceCensus, DeviceState, DriverBindingManifest, EVIDENCE_CLASS_TUPLE, EVIDENCE_IDENTITY,
    EVIDENCE_PCI_CONFIGURATION, MAXIMUM_DISPLAY_CLAIMS, boot_device_record,
};
use boulder::drivers::drivernet::fingerprint::LegacyConfigurationReader;
use boulder::drivers::xhci::{
    XHCI_PROBE_DRIVER_ID, XhciMutationDebt, XhciProbeCensus, activate_reset_ready,
    activation_containment_root as xhci_activation_containment_root, boot_xhci_snapshot,
    boot_xhci_summary, boot_xhci_terminal_root, containment_root as xhci_containment_root,
    probe_bootstrap, publish_boot_xhci,
};
use boulder::fabric::{
    Completion, KERNEL_FABRIC, NodeCapabilities, NodeClass, WorkDescriptor, opcode,
};
use boulder::hw::pci;
use boulder::ignition::{BootProtocol, IgnitionSequence};
use boulder::interrupts;
use boulder::memory::frame_pool::PhysicalFramePool;
use boulder::mmio::{
    EARLY_MAPPED_PHYSICAL_LIMIT, HIGHER_HALF_DIRECT_MAP_BASE, KERNEL_VIRTUAL_BASE,
    direct_map_address, kernel_mmio,
};
use boulder::process::install::UserAddressSpaceBackend;
use boulder::process::lifecycle::{self, ProcessLaunch};
use boulder::process::x86_64::{
    DirectMapFrameMemory, FrameBackedAddressSpace, INITIAL_USER_STACK_PAGES,
};
use boulder::ring_authority::{
    DomainDescriptor, DomainRegistry, DomainRole, HardwareAuthority, TransitionFrontier,
    TransitionGate,
};
use boulder::serial::SerialPort;
use boulder::shim::{
    AbyssAllocator, DriverHost, DriverServices, IrqService, LogService, MmioService,
};
use boulder::sync::SpinLock;
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

struct BootDriverLogger<'a> {
    serial: SpinLock<&'a mut SerialPort>,
}

impl<'a> BootDriverLogger<'a> {
    fn new(serial: &'a mut SerialPort) -> Self {
        Self {
            serial: SpinLock::new(serial),
        }
    }
}

impl LogService for BootDriverLogger<'_> {
    fn log(&self, level: u32, message: &[u8]) -> sisyphus_driver_abi::Status {
        let mut serial = self.serial.lock();
        let _ = write!(serial, "Boulder: C driver log level {level}: ");
        serial.write_bytes(message);
        serial.write_bytes(b"\n");
        sisyphus_driver_abi::STATUS_OK
    }
}

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
    let idt_info = match unsafe { interrupts::initialize() } {
        Ok(info) => info,
        Err(error) => {
            let _ = writeln!(serial, "Boulder: interrupt tables failed: {error:?}");
            halt();
        }
    };
    if !interrupts::trigger_ist_probe() {
        let _ = writeln!(serial, "Boulder: IST runtime probe failed");
        halt();
    }
    interrupts::trigger_breakpoint();
    if interrupts::breakpoint_hits() != 1 {
        let _ = writeln!(serial, "Boulder: breakpoint exception test failed");
        halt();
    }
    let (local_apic, x2apic) = interrupts::apic_capabilities();
    let _ = writeln!(
        serial,
        "Boulder: IDT active, IST runtime probe verified, DF/NMI/MC={}@{:#x}/{}@{:#x}/{}@{:#x}, local APIC={}, x2APIC={}",
        idt_info.double_fault_ist,
        idt_info.fault_stacks.double_fault.top,
        idt_info.non_maskable_interrupt_ist,
        idt_info.fault_stacks.non_maskable_interrupt.top,
        idt_info.machine_check_ist,
        idt_info.fault_stacks.machine_check.top,
        local_apic,
        x2apic
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
    let boot_framebuffer = match boot.framebuffer() {
        Ok(framebuffer) => framebuffer,
        Err(boulder::boot::multiboot2::BootError::UnsupportedFramebuffer) => {
            let _ = writeln!(
                serial,
                "Boulder: firmware framebuffer format unsupported; continuing headless"
            );
            None
        }
        Err(error) => {
            let _ = writeln!(serial, "Boulder: framebuffer tag rejected: {error:?}");
            halt();
        }
    };
    if let Some(framebuffer) = boot_framebuffer {
        let _ = writeln!(
            serial,
            "Boulder: firmware framebuffer {:#x}..{:#x} {}x{} pitch={} format={}",
            framebuffer.physical_address,
            framebuffer.end().unwrap_or(framebuffer.physical_address),
            framebuffer.width,
            framebuffer.height,
            framebuffer.pitch,
            framebuffer.format,
        );
    } else {
        let _ = writeln!(serial, "Boulder: no supported firmware framebuffer");
    }
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

    if let Some(framebuffer) = boot_framebuffer {
        if framebuffer.physical_address < IDENTITY_MAP_END {
            let end = framebuffer
                .end()
                .unwrap_or(framebuffer.physical_address)
                .min(IDENTITY_MAP_END);
            if end > framebuffer.physical_address {
                if let Err(error) = reservations.push(Reservation::new(
                    PhysicalAddress::new(framebuffer.physical_address),
                    PhysicalAddress::new(end),
                    ReservationKind::DeviceMemory,
                )) {
                    let _ = writeln!(serial, "Abyss: framebuffer reservation failed: {error:?}");
                    halt();
                }
            }
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
    let frame_pool = PhysicalFramePool::new(frames);

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
    if let Err(error) = ignition.memory_ready(frame_pool.managed_frames(), frame_pool.free_frames())
    {
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
    // SAFETY: Uses the same bounded, stable ACPI mapping as MADT discovery.
    // A malformed optional table disables remapping evidence instead of
    // manufacturing isolation or preventing a firmware-only boot.
    let dmar = match unsafe { discover_dmar(rsdp, map_acpi_region) } {
        Ok(dmar) => dmar,
        Err(error) => {
            let _ = writeln!(
                serial,
                "Boulder: ACPI DMAR rejected; native DMA remains disabled: {error:?}"
            );
            None
        }
    };
    if let Some(dmar) = dmar.as_ref() {
        let _ = writeln!(
            serial,
            "Boulder: DMAR host-width={} units={}, presence-only",
            dmar.host_address_width,
            dmar.remapping_units().len(),
        );
    }

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
    // SAFETY: The self-IPI test restored disabled interrupts, and no subsystem
    // has claimed PIT channel 2 or the local APIC timer. Retain this one-shot
    // owner across hardware discovery so bounded takeover work can be inserted
    // without calibrating a second, unrelated clock.
    let mut deadline_clock = match unsafe { interrupts::initialize_local_apic_deadline_clock() } {
        Ok(clock) => clock,
        Err(error) => {
            let _ = writeln!(
                serial,
                "Boulder: local APIC deadline calibration failed: {error:?}"
            );
            halt();
        }
    };
    let _ = writeln!(
        serial,
        "Boulder: local APIC deadline clock {} Hz reserved",
        deadline_clock.ticks_per_second()
    );
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
    if let Some(dmar) = dmar.as_ref() {
        let device_memory = authority.grant::<DeviceMemoryControl>();
        for unit in dmar.remapping_units().iter().copied() {
            let registers = match boulder::hw::vtd::VtdMmioRegisters::map(unit, &device_memory) {
                Ok(registers) => registers,
                Err(error) => {
                    let _ = writeln!(
                        serial,
                        "Boulder: VT-d unit {:#x} MMIO rejected: {error:?}",
                        unit.register_base
                    );
                    continue;
                }
            };
            let engine = match registers.into_engine() {
                Ok(engine) => engine,
                Err(failure) => {
                    let fault = failure.fault();
                    let registers = failure.into_registers();
                    let close = registers.close(&device_memory);
                    let _ = writeln!(
                        serial,
                        "Boulder: VT-d unit {:#x} probe rejected: {fault:?}, close={close:?}",
                        unit.register_base
                    );
                    continue;
                }
            };
            let version = engine.version();
            let capabilities = engine.capabilities();
            let state = engine.state();
            let registers = match engine.into_registers() {
                Ok(registers) => registers,
                Err(_) => {
                    let _ = writeln!(
                        serial,
                        "Boulder: VT-d unit {:#x} retained unexpected live authority",
                        unit.register_base
                    );
                    continue;
                }
            };
            let close = registers.close(&device_memory);
            let _ = writeln!(
                serial,
                "Boulder: VT-d unit {:#x} v{}.{} state={state:?} sagaw={:#x} mgaw={} close={close:?}",
                unit.register_base,
                version.major,
                version.minor,
                capabilities.supported_adjusted_guest_widths,
                capabilities.maximum_guest_address_width,
            );
        }
    }
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
            &frame_pool,
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

    let cerebral_lease = match boulder::nexus_runtime::initialize(&resonance_control) {
        Ok(token) => token,
        Err(error) => {
            let _ = writeln!(
                serial,
                "Boulder: Nexus runtime initialization failed: {error:?}"
            );
            halt();
        }
    };
    if let Err(error) = boulder::nexus_plane::initialize(&learning_control, cerebral_lease) {
        let _ = writeln!(
            serial,
            "Boulder: Nexus plane initialization failed: {error:?}"
        );
        halt();
    }

    {
        if let Err(error) = process_backend.install_nexus_plane(&pid1, &process_install) {
            let _ = writeln!(
                serial,
                "Boulder: PID1 nexus plane mapping failed: {error:?}"
            );
            halt();
        }
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

    // Drivernet: derive a per-boot, measurement-bound control domain before
    // collapsing the GPU strategy set. No driver key is a repeated literal.
    let gpu_boot_counter = <boulder::arch::Active as boulder::arch::Architecture>::counter_sample();
    let gpu_domains = match boulder::drivernet_host::derive_gpu_boot_domains(
        PUSH_EXPECTED_SHA256,
        gpu_boot_counter,
        &pci_inventory,
        boot_framebuffer,
    ) {
        Ok(domains) => domains,
        Err(error) => {
            let _ = writeln!(
                serial,
                "Boulder: GPU boot-domain derivation failed: {error:?}"
            );
            halt();
        }
    };
    let census_secret = gpu_domains.drivernet.fingerprint.rotate_left(17) | 1;
    let mut device_census =
        match BootDeviceCensus::measure_pci(&pci_inventory, dmar.as_ref(), census_secret) {
            Ok(census) => census,
            Err(error) => {
                let _ = writeln!(serial, "Boulder: device census failed: {error:?}");
                halt();
            }
        };
    let display_route = DriverBindingManifest {
        driver_id: 0x4452_4956_4552_4e45,
        family: boulder::drivers::device_census::DeviceFamily::DisplayAdapter,
        vendor_id: 0xffff,
        device_id_mask: 0,
        device_id_value: 0,
        class_code_mask: u8::MAX,
        class_code_value: 0x03,
        subclass_mask: 0,
        subclass_value: 0,
        programming_interface_mask: 0,
        programming_interface_value: 0,
        revision_minimum: 0,
        revision_maximum: u8::MAX,
        required_evidence: EVIDENCE_IDENTITY | EVIDENCE_CLASS_TUPLE | EVIDENCE_PCI_CONFIGURATION,
        requested_authority: AUTHORITY_DELEGATE,
    };
    let display_claims = match device_census
        .claim_family::<MAXIMUM_DISPLAY_CLAIMS>(display_route, AUTHORITY_DELEGATE)
    {
        Ok(claims) => claims,
        Err(error) => {
            let _ = writeln!(serial, "Boulder: display routing claim failed: {error:?}");
            halt();
        }
    };
    let xhci_route = DriverBindingManifest {
        driver_id: XHCI_PROBE_DRIVER_ID,
        family: boulder::drivers::device_census::DeviceFamily::UsbHostController,
        vendor_id: 0xffff,
        device_id_mask: 0,
        device_id_value: 0,
        class_code_mask: u8::MAX,
        class_code_value: 0x0c,
        subclass_mask: u8::MAX,
        subclass_value: 0x03,
        programming_interface_mask: u8::MAX,
        programming_interface_value: 0x30,
        revision_minimum: 0,
        revision_maximum: u8::MAX,
        required_evidence: EVIDENCE_IDENTITY | EVIDENCE_CLASS_TUPLE | EVIDENCE_PCI_CONFIGURATION,
        requested_authority: AUTHORITY_MMIO
            | AUTHORITY_DMA
            | AUTHORITY_CLOCK
            | AUTHORITY_PCI_CONFIG,
    };
    let xhci_claims = match device_census
        .claim_family::<{ boulder::drivers::xhci::MAXIMUM_XHCI_CONTROLLERS }>(
            xhci_route,
            AUTHORITY_MMIO | AUTHORITY_DMA | AUTHORITY_CLOCK | AUTHORITY_PCI_CONFIG,
        ) {
        Ok(claims) => claims,
        Err(error) => {
            let _ = writeln!(serial, "Boulder: xHCI routing claim failed: {error:?}");
            halt();
        }
    };
    let detected_devices = device_census.summary();
    let _ = writeln!(
        serial,
        "Boulder: device census detected total={} display={} audio={} multimedia-video={} network={} wireless={} usb-host={} input={} other={} root={:#x}",
        detected_devices.total,
        detected_devices.display,
        detected_devices.audio,
        detected_devices.multimedia_video,
        detected_devices.network,
        detected_devices.wireless,
        detected_devices.usb_hosts,
        detected_devices.input,
        detected_devices.other,
        detected_devices.root,
    );
    let xhci_secret = census_secret.rotate_left(23) | 1;
    let mut xhci_census = match XhciProbeCensus::new(xhci_secret) {
        Ok(census) => census,
        Err(error) => {
            let _ = writeln!(serial, "Boulder: xHCI census creation failed: {error:?}");
            halt();
        }
    };
    let configuration = LegacyConfigurationReader;
    let xhci_mmio = authority.grant::<DeviceMemoryControl>();
    let xhci_pci_configuration = authority.grant::<PciConfigurationControl>();
    for claim in xhci_claims.claims().iter().copied() {
        let address = claim.address();
        let Some(evidence) = device_census
            .evidence()
            .find(|evidence| evidence.address == address)
            .copied()
        else {
            let _ = writeln!(serial, "Boulder: xHCI claim lost its device evidence");
            halt();
        };
        let authorization = match device_census.authorize(
            claim,
            XHCI_PROBE_DRIVER_ID,
            AUTHORITY_MMIO | AUTHORITY_DMA | AUTHORITY_CLOCK | AUTHORITY_PCI_CONFIG,
        ) {
            Ok(authorization) => authorization,
            Err(error) => {
                let _ = writeln!(serial, "Boulder: xHCI live authorization failed: {error:?}");
                halt();
            }
        };
        match probe_bootstrap(
            authorization,
            evidence,
            &configuration,
            &xhci_mmio,
            xhci_secret,
        ) {
            Ok(bootstrap) => {
                match activate_reset_ready(
                    bootstrap,
                    &mut deadline_clock,
                    &xhci_mmio,
                    &xhci_pci_configuration,
                    xhci_secret,
                ) {
                    Ok(controller) => {
                        let snapshot = controller.snapshot();
                        let reset_ready_root = controller.reset_ready_root();
                        let aperture_bytes = controller.aperture().length();
                        let legacy = controller.ready().legacy_handoff_performed();
                        let protocol_count = controller.protocols().protocol_count();
                        let usb2_ports = controller
                            .protocols()
                            .usb2_protocols()
                            .map(|protocol| usize::from(protocol.port_count))
                            .sum::<usize>();
                        let usb3_ports = controller
                            .protocols()
                            .usb3_protocols()
                            .map(|protocol| usize::from(protocol.port_count))
                            .sum::<usize>();
                        if let Err(error) = xhci_census.insert_reset_ready(controller) {
                            let _ = writeln!(
                                serial,
                                "Boulder: xHCI reset-ready retention failed: {error:?}"
                            );
                            halt();
                        }
                        if let Err(error) = device_census.defer(claim, reset_ready_root) {
                            let _ = writeln!(serial, "Boulder: xHCI deferral failed: {error:?}");
                            halt();
                        }
                        let _ = writeln!(
                            serial,
                            "Boulder: xHCI reset-ready {:?} bar0-bytes={} legacy-handoff={} protocols={} usb2-ports={} usb3-ports={} halted=true bus-master=false root={:#x}",
                            snapshot.address,
                            aperture_bytes,
                            legacy,
                            protocol_count,
                            usb2_ports,
                            usb3_ports,
                            reset_ready_root,
                        );
                    }
                    Err(failure) => {
                        let snapshot = failure.snapshot();
                        let phase = failure.phase();
                        let debt_class = failure.debt_class();
                        let mutated = failure.mutated();
                        let _ = writeln!(
                            serial,
                            "Boulder: xHCI takeover contained {:?} phase={phase:?} mutated={mutated}: {:?}",
                            snapshot.address,
                            failure.error(),
                        );
                        let containment_root = if mutated {
                            let (bootstrap, aperture) = failure.into_parts();
                            let debt = match XhciMutationDebt::retain(
                                bootstrap,
                                aperture,
                                phase,
                                debt_class,
                                xhci_secret,
                            ) {
                                Ok(debt) => debt,
                                Err(error) => {
                                    let _ = writeln!(
                                        serial,
                                        "Boulder: xHCI mutation debt retention failed: {error:?}"
                                    );
                                    halt();
                                }
                            };
                            let root = debt.debt_root(xhci_secret);
                            if root == 0 {
                                let _ =
                                    writeln!(serial, "Boulder: xHCI mutation debt audit failed");
                                halt();
                            }
                            if let Err(error) = xhci_census.insert_mutation_debt(debt) {
                                let _ = writeln!(
                                    serial,
                                    "Boulder: xHCI mutation debt census failed: {error:?}"
                                );
                                halt();
                            }
                            root
                        } else {
                            let Some(root) = xhci_activation_containment_root(
                                xhci_secret,
                                snapshot,
                                phase,
                                debt_class,
                                false,
                            ) else {
                                let _ = writeln!(
                                    serial,
                                    "Boulder: xHCI activation containment sealing failed"
                                );
                                halt();
                            };
                            if let Err(error) = xhci_census.insert(snapshot) {
                                let _ = writeln!(
                                    serial,
                                    "Boulder: xHCI failed snapshot retention failed: {error:?}"
                                );
                                halt();
                            }
                            root
                        };
                        if let Err(error) = device_census.quarantine(claim, containment_root) {
                            let _ = writeln!(serial, "Boulder: xHCI quarantine failed: {error:?}");
                            halt();
                        }
                    }
                }
            }
            Err(error) => {
                let _ = writeln!(
                    serial,
                    "Boulder: xHCI read-only probe quarantined {:?}: {error:?}",
                    address,
                );
                let Some(containment_root) =
                    xhci_containment_root(xhci_secret, claim.evidence_root(), address, error)
                else {
                    let _ = writeln!(serial, "Boulder: xHCI containment sealing failed");
                    halt();
                };
                if let Err(containment) = device_census.quarantine(claim, containment_root) {
                    let _ = writeln!(serial, "Boulder: xHCI quarantine failed: {containment:?}");
                    halt();
                }
            }
        }
    }
    let xhci_summary = match publish_boot_xhci(xhci_census) {
        Ok(summary) => summary,
        Err(error) => {
            let _ = writeln!(serial, "Boulder: xHCI census publication failed: {error:?}");
            halt();
        }
    };
    if boot_xhci_summary() != Some(xhci_summary) {
        let _ = writeln!(serial, "Boulder: retained xHCI census verification failed");
        halt();
    }
    let _ = writeln!(
        serial,
        "Boulder: xHCI capability census controllers={} ports={} slots={} bootstrap-headers={} legacy-capable={} reset-ready={} aperture-bytes={} protocols={} usb2-ports={} usb3-ports={} debt={} deferred=true root={:#x}",
        xhci_summary.controllers,
        xhci_summary.total_ports,
        xhci_summary.total_slots,
        xhci_summary.bootstrap_headers,
        xhci_summary.legacy_capable_controllers,
        xhci_summary.reset_ready_controllers,
        xhci_summary.measured_aperture_bytes,
        xhci_summary.supported_protocols,
        xhci_summary.usb2_ports,
        xhci_summary.usb3_ports,
        xhci_summary.mutation_debts,
        xhci_summary.root,
    );
    let mut blacklab_complex =
        match boulder::blacklab_bootstrap::KernelBlackLabComplex::new(gpu_domains.blacklab) {
            Ok(complex) => complex,
            Err(error) => {
                let _ = writeln!(serial, "Boulder: Blacklab bootstrap failed: {error:?}");
                halt();
            }
        };
    let blacklab_policy = authority.grant::<boulder::capability::PolicyControl>();
    if let Err(error) = blacklab_complex.install_default_rules(&blacklab_policy) {
        let _ = writeln!(serial, "Boulder: Blacklab rule install failed: {error:?}");
        halt();
    }
    let drivernet = match boulder::drivernet_host::resolve_drivernet(
        &pci_inventory,
        dmar.as_ref(),
        boot_framebuffer,
        gpu_domains.drivernet,
        &authority,
        &mut blacklab_complex,
    ) {
        Ok(summary) => summary,
        Err(error) => {
            let _ = writeln!(serial, "Boulder: Drivernet resolution failed: {error:?}");
            halt();
        }
    };
    let _ = writeln!(
        serial,
        "Boulder: drivernet resolved {} GPU slot(s), {} display function(s)",
        drivernet.length, drivernet.fingerprint_summary.display_functions
    );
    for claim in display_claims.claims().iter().copied() {
        let address = claim.address();
        let resolution = drivernet.resolutions().iter().find(|resolution| {
            resolution.fingerprint.segment == address.segment
                && resolution.fingerprint.bus == address.bus
                && resolution.fingerprint.slot == address.slot
                && resolution.fingerprint.function == address.function
        });
        match resolution {
            Some(resolution)
                if resolution.status
                    == boulder::drivers::drivernet::GpuResolutionStatus::Committed =>
            {
                if let Err(error) = device_census.commit(claim, resolution.resolution_root) {
                    let _ = writeln!(serial, "Boulder: display binding commit failed: {error:?}");
                    halt();
                }
            }
            Some(resolution) => {
                let containment_root = if resolution.resolution_root != 0 {
                    resolution.resolution_root
                } else {
                    resolution.decision_root
                };
                if let Err(error) = device_census.quarantine(claim, containment_root) {
                    let _ = writeln!(
                        serial,
                        "Boulder: display binding quarantine failed: {error:?}"
                    );
                    halt();
                }
            }
            None => {
                if let Err(error) = device_census.quarantine(claim, detected_devices.root) {
                    let _ = writeln!(
                        serial,
                        "Boulder: unresolved display containment failed: {error:?}"
                    );
                    halt();
                }
            }
        }
    }
    let device_summary = match boulder::drivers::device_census::publish_boot_census(device_census) {
        Ok(summary) => summary,
        Err(error) => {
            let _ = writeln!(
                serial,
                "Boulder: device census publication failed: {error:?}"
            );
            halt();
        }
    };
    if boulder::drivers::device_census::boot_census_summary() != Some(device_summary) {
        let _ = writeln!(
            serial,
            "Boulder: retained device census verification failed"
        );
        halt();
    }
    for claim in xhci_claims.claims().iter().copied() {
        let address = claim.address();
        let Some(record) = boot_device_record(address) else {
            let _ = writeln!(serial, "Boulder: retained xHCI device record missing");
            halt();
        };
        match boot_xhci_snapshot(address) {
            Some(snapshot)
                if matches!(
                    record.state,
                    DeviceState::Deferred | DeviceState::Quarantined
                ) && record.driver_id == XHCI_PROBE_DRIVER_ID
                    && record.authority
                        & (AUTHORITY_MMIO | AUTHORITY_CLOCK | AUTHORITY_PCI_CONFIG)
                        == (AUTHORITY_MMIO | AUTHORITY_CLOCK | AUTHORITY_PCI_CONFIG)
                    && boot_xhci_terminal_root(address) == Some(record.terminal_root)
                    && record.evidence.evidence_root == snapshot.evidence_root
                    && snapshot.binding_root != 0 =>
            {
                // The retained transport prerequisite and the retained device
                // binding name the same measured PCI function.
            }
            None if record.state == DeviceState::Quarantined => {}
            _ => {
                let _ = writeln!(serial, "Boulder: retained xHCI evidence diverged");
                halt();
            }
        }
    }
    let _ = writeln!(
        serial,
        "Boulder: device bindings retained detected={} claimed={} operational={} quarantined={} deferred={} root={:#x}",
        device_summary.detected,
        device_summary.claimed,
        device_summary.operational,
        device_summary.quarantined,
        device_summary.deferred,
        device_summary.root,
    );

    if let Some(primary) = drivernet.primary().filter(|resolution| {
        resolution.strategy
            == boulder::drivers::drivernet::model::DriverStrategy::FirmwareFramebuffer
            && resolution.framebuffer_object != 0
    }) {
        let device_memory = authority.grant::<DeviceMemoryControl>();
        match boulder::drivers::firmware_display::render_boot_signature(
            primary.framebuffer_object,
            &device_memory,
        ) {
            Ok(report) => {
                let _ = writeln!(
                    serial,
                    "Boulder: firmware scanout verified object={:#x} generation={} pixels={} samples={} root={:#x}",
                    report.object,
                    report.generation,
                    report.pixels_written,
                    report.pixels_verified,
                    report.image_root,
                );
            }
            Err(error) => {
                let _ = writeln!(
                    serial,
                    "Boulder: firmware scanout verification failed: {error:?}"
                );
                halt();
            }
        }
    }

    // Manifold: PCI/drivernet → cluster quiver → Hodge Δ₁ → NTT64 fairq
    boulder::manifold_orchestrator::boot_after_drivernet(
        &pci_inventory,
        &drivernet,
        PUSH_EXPECTED_SHA256,
        &mut serial,
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

    let timer = match deadline_clock.start_periodic(interrupts::APIC_TIMER_VECTOR, 10) {
        Ok(timer) => timer,
        Err((error, _deadline_clock)) => {
            let _ = writeln!(
                serial,
                "Boulder: local APIC periodic transition failed: {error:?}"
            );
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

    let driver_capabilities;
    #[cfg(feature = "reference-driver")]
    let reference_driver_result;
    {
        let driver_logger = BootDriverLogger::new(&mut serial);
        let driver_services = DriverServices::new()
            .with_logger(&driver_logger)
            .with_allocator(&driver_allocator)
            .with_mmio(mmio)
            .with_irq(irq);
        let driver_host = DriverHost::new(&driver_services);
        driver_capabilities = driver_host.api().capabilities;

        #[cfg(feature = "reference-driver")]
        {
            reference_driver_result = (|| {
                let module = boulder::shim::linked_reference_driver(driver_host.api())?;
                let address = b"platform:reference0";
                let device = sisyphus_driver_abi::DeviceInfo {
                    struct_size: core::mem::size_of::<sisyphus_driver_abi::DeviceInfo>() as u32,
                    bus_type: sisyphus_driver_abi::BUS_PLATFORM,
                    kernel_handle: 1,
                    vendor_id: 0,
                    device_id: 0,
                    subsystem_vendor_id: 0,
                    subsystem_device_id: 0,
                    class_code: 0,
                    revision: 0,
                    address: address.as_ptr(),
                    address_len: address.len(),
                };
                let mut instance = module.probe_with_api(driver_host.api(), &device)?;
                if module
                    .remove_with_api(driver_host.api(), &device, &mut instance)
                    .is_err()
                {
                    module.remove_with_api(driver_host.api(), &device, &mut instance)?;
                }
                Ok::<(), boulder::shim::DriverLoadError>(())
            })();
        }
    }
    let _ = writeln!(
        serial,
        "Boulder: driver host capabilities {:#x}",
        driver_capabilities
    );
    #[cfg(feature = "reference-driver")]
    match reference_driver_result {
        Ok(()) => {
            let _ = writeln!(serial, "Boulder: linked C driver lifecycle verified");
        }
        Err(error) => {
            let _ = writeln!(serial, "Boulder: linked C driver failed: {error:?}");
            halt();
        }
    }
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

    let formal_attestation = boulder::formal_attestation::FormalAttestation::current();
    if !formal_attestation.validate() {
        let _ = writeln!(serial, "Boulder: formal authority attestation rejected");
        halt();
    }
    let _ = writeln!(
        serial,
        "Boulder: Idris/Agda authority root {:#x} bound to PID1",
        formal_attestation.authority_root,
    );

    let mut image_measurement_root = 0_u64;
    for (index, chunk) in PUSH_EXPECTED_SHA256.chunks_exact(8).enumerate() {
        let word = u64::from_le_bytes([
            chunk[0], chunk[1], chunk[2], chunk[3], chunk[4], chunk[5], chunk[6], chunk[7],
        ]);
        image_measurement_root ^= word.rotate_left((index as u32) * 13);
    }
    image_measurement_root = image_measurement_root.max(1);
    let capability_root = (image_measurement_root
        ^ blacklab.evolution_generation.rotate_left(29)
        ^ blacklab.next_epoch.rotate_left(47)
        ^ formal_attestation.authority_root.rotate_left(7)
        ^ u64::from(blacklab.pid1_install_generation))
    .max(1);
    let pid1_launch = ProcessLaunch {
        address_space_root: pid1_root,
        entry_point: pid1_info.entry_point,
        user_stack_pointer: pid1_stack,
        kernel_stack_pointer: privilege_info.kernel_stack_top as u64,
        image_measurement_root,
        capability_root,
        service_class: 1,
        priority: u8::MAX,
    };
    let pid1_handle = match lifecycle::register_init(pid1_launch) {
        Ok(handle) => handle,
        Err(error) => {
            let _ = writeln!(
                serial,
                "Boulder: measured PID1 registration failed: {error:?}"
            );
            halt();
        }
    };
    let pid1_snapshot = match lifecycle::mark_running(pid1_handle) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            let _ = writeln!(
                serial,
                "Boulder: measured PID1 activation failed: {error:?}"
            );
            halt();
        }
    };
    if pid1_snapshot.launch != pid1_launch
        || lifecycle::current_handle() != Some(pid1_handle)
        || pid1_handle.pid != lifecycle::INIT_PID
    {
        let _ = writeln!(
            serial,
            "Boulder: measured PID1 authority publication failed"
        );
        halt();
    }
    let mut ring_registry = match DomainRegistry::<4>::new(kernel_page_table_root.as_u64()) {
        Ok(registry) => registry,
        Err(error) => {
            let _ = writeln!(
                serial,
                "Boulder: privilege-domain registry failed: {error:?}"
            );
            halt();
        }
    };
    let pid1_domain = match ring_registry.register(DomainDescriptor {
        role: DomainRole::UserProcess,
        address_space_root: pid1_root,
        authority: HardwareAuthority::NONE,
    }) {
        Ok(domain) => domain,
        Err(error) => {
            let _ = writeln!(serial, "Boulder: PID1 Ring 3 domain failed: {error:?}");
            halt();
        }
    };
    let mut ring_frontier = match TransitionFrontier::new(
        privilege_info.logical_cpu_id,
        privilege_info.cpu_generation,
        kernel_page_table_root.as_u64(),
    ) {
        Ok(frontier) => frontier,
        Err(error) => {
            let _ = writeln!(
                serial,
                "Boulder: privilege transition frontier failed: {error:?}"
            );
            halt();
        }
    };
    let _ = writeln!(
        serial,
        "Boulder: transferring to measured Push PID1 authority {}:{} through Ring 3 domain {}:{} at {:#x}, measurement={:#x}",
        pid1_handle.pid,
        pid1_handle.generation,
        pid1_domain.slot(),
        pid1_domain.generation(),
        pid1_snapshot.launch.entry_point,
        pid1_snapshot.launch.image_measurement_root,
    );
    interrupts::disable();
    let transition_lease =
        match ring_frontier.prepare(&mut ring_registry, pid1_domain, TransitionGate::Iretq) {
            Ok(lease) => lease,
            Err(error) => {
                let _ = writeln!(serial, "Boulder: PID1 Ring 3 preparation failed: {error:?}");
                halt();
            }
        };
    let observed_kernel_root = unsafe { active_page_table_root() };
    let committed_transition =
        match ring_frontier.commit(&ring_registry, &transition_lease, observed_kernel_root) {
            Ok(transition) => transition,
            Err(error) => {
                let _ = ring_frontier.abort(&mut ring_registry, transition_lease);
                let _ = writeln!(serial, "Boulder: PID1 Ring 3 commit failed: {error:?}");
                halt();
            }
        };
    // SAFETY: Push's measured W^X image, retained hierarchy, and RW+NX stack
    // remain owned by `process_backend`, all kernel entry mappings are
    // inherited, and this terminal transfer intentionally abandons the
    // bootstrap stack without running destructors.
    if let Err(error) = unsafe {
        privilege::enter_user_process(
            pid1_info.entry_point as usize,
            pid1_stack as usize,
            committed_transition,
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
