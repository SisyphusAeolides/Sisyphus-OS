#ifndef SISYPHUS_GPU_H
#define SISYPHUS_GPU_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#define SISY_GPU_ABI_MAJOR 1u
#define SISY_GPU_ABI_MINOR 0u
#define SISY_GPU_ABI_VERSION \
    ((SISY_GPU_ABI_MAJOR << 16u) | SISY_GPU_ABI_MINOR)

#define SISY_GPU_BAR_COUNT 6u

#define SISY_GPU_BAR_PRESENT (UINT32_C(1) << 0u)
#define SISY_GPU_BAR_IO (UINT32_C(1) << 1u)
#define SISY_GPU_BAR_64BIT (UINT32_C(1) << 2u)
#define SISY_GPU_BAR_PREFETCHABLE (UINT32_C(1) << 3u)

#define SISY_GPU_TOPOLOGY_BOOT_DISPLAY (UINT64_C(1) << 0u)
#define SISY_GPU_TOPOLOGY_IOMMU_PRESENT (UINT64_C(1) << 1u)
#define SISY_GPU_TOPOLOGY_IOMMU_ISOLATED (UINT64_C(1) << 2u)
#define SISY_GPU_TOPOLOGY_FIRMWARE_SURFACE (UINT64_C(1) << 3u)
#define SISY_GPU_TOPOLOGY_VIRTUAL_MACHINE (UINT64_C(1) << 4u)
#define SISY_GPU_TOPOLOGY_HOTPLUG (UINT64_C(1) << 5u)
#define SISY_GPU_TOPOLOGY_INVENTORY_COMPLETE (UINT64_C(1) << 6u)

#define SISY_GPU_FIRMWARE_REQUIRED (UINT32_C(1) << 0u)
#define SISY_GPU_FIRMWARE_FORBIDDEN (UINT32_C(1) << 1u)
#define SISY_GPU_FIRMWARE_PRESERVE_BOOT_SURFACE (UINT32_C(1) << 2u)
#define SISY_GPU_FIRMWARE_AUTHENTICATED (UINT32_C(1) << 3u)

#define SISY_GPU_DRIVER_NATIVE UINT8_C(1)
#define SISY_GPU_DRIVER_PARAVIRTUAL UINT8_C(2)
#define SISY_GPU_DRIVER_FIRMWARE_SURFACE UINT8_C(3)
#define SISY_GPU_DRIVER_FOREIGN_PERSONALITY UINT8_C(4)

#define SISY_GPU_COMPATIBILITY_REJECTED UINT8_C(0)
#define SISY_GPU_COMPATIBILITY_ACCEPTED UINT8_C(1)

struct sisy_gpu_pci_identity {
    uint16_t segment;
    uint8_t bus;
    uint8_t slot;
    uint8_t function;
    uint8_t revision;
    uint16_t vendor_id;
    uint16_t device_id;
    uint16_t subsystem_vendor_id;
    uint16_t subsystem_device_id;
    uint8_t class_code;
    uint8_t subclass;
    uint8_t programming_interface;
    uint8_t reserved;
};

struct sisy_gpu_bar_evidence {
    uint64_t physical_address;
    uint64_t length;
    uint32_t flags;
    uint32_t reserved;
};

struct sisy_gpu_firmware_surface {
    uint64_t physical_address;
    uint64_t byte_length;
    uint32_t width;
    uint32_t height;
    uint32_t pitch;
    uint32_t format;
    uint32_t flags;
    uint32_t reserved;
};

struct sisy_gpu_device_evidence {
    uint32_t abi_version;
    uint32_t struct_size;
    struct sisy_gpu_pci_identity identity;
    struct sisy_gpu_bar_evidence bars[SISY_GPU_BAR_COUNT];
    uint64_t capability_flags;
    uint64_t topology_flags;
    uint64_t observed_features;
    uint32_t architecture_hint;
    uint32_t bootrom_revision;
    struct sisy_gpu_firmware_surface firmware_surface;
    uint64_t evidence_root;
};

struct sisy_gpu_compatibility_manifest {
    uint32_t abi_version;
    uint32_t struct_size;
    uint64_t driver_id;
    uint8_t driver_class;
    uint8_t reserved0[7];
    uint16_t vendor_id;
    uint16_t device_id_mask;
    uint16_t device_id_value;
    uint8_t class_mask;
    uint8_t class_value;
    uint8_t subclass_mask;
    uint8_t subclass_value;
    uint8_t revision_minimum;
    uint8_t revision_maximum;
    uint16_t reserved1;
    uint32_t architecture_mask;
    uint32_t architecture_value;
    uint64_t required_topology;
    uint64_t forbidden_topology;
    uint64_t required_features;
    uint64_t optional_features;
    uint8_t required_bar_mask;
    uint8_t reserved2[7];
    uint64_t minimum_bar_lengths[SISY_GPU_BAR_COUNT];
    uint32_t firmware_policy;
    uint16_t priority;
    uint16_t reserved3;
};

struct sisy_gpu_compatibility_proof {
    uint64_t driver_id;
    uint64_t evidence_root;
    uint64_t satisfied_obligations;
    uint64_t missing_obligations;
    uint64_t violated_obligations;
    uint64_t matched_optional_features;
    uint32_t score_q16;
    uint8_t verdict;
    uint8_t reserved[3];
    uint64_t proof_root;
};

#if defined(__STDC_VERSION__) && __STDC_VERSION__ >= 201112L
_Static_assert(sizeof(struct sisy_gpu_pci_identity) == 18u,
               "sisy_gpu_pci_identity layout mismatch");
_Static_assert(sizeof(struct sisy_gpu_bar_evidence) == 24u,
               "sisy_gpu_bar_evidence layout mismatch");
_Static_assert(sizeof(struct sisy_gpu_firmware_surface) == 40u,
               "sisy_gpu_firmware_surface layout mismatch");
_Static_assert(sizeof(struct sisy_gpu_device_evidence) == 256u,
               "sisy_gpu_device_evidence layout mismatch");
_Static_assert(sizeof(struct sisy_gpu_compatibility_manifest) == 144u,
               "sisy_gpu_compatibility_manifest layout mismatch");
_Static_assert(sizeof(struct sisy_gpu_compatibility_proof) == 64u,
               "sisy_gpu_compatibility_proof layout mismatch");
#endif

#ifdef __cplusplus
}
#endif

#endif
