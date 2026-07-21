#ifndef SISYPHUS_DRIVER_H
#define SISYPHUS_DRIVER_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#define SISY_ABI_MAJOR 1u
#define SISY_ABI_MINOR 0u
#define SISY_ABI_VERSION ((SISY_ABI_MAJOR << 16u) | SISY_ABI_MINOR)

#define SISY_STATUS_OK 0
#define SISY_STATUS_INVALID_ARGUMENT (-1)
#define SISY_STATUS_UNSUPPORTED (-2)
#define SISY_STATUS_NO_MEMORY (-3)
#define SISY_STATUS_BUSY (-4)
#define SISY_STATUS_NOT_FOUND (-5)
#define SISY_STATUS_IO_ERROR (-6)
#define SISY_STATUS_ABI_MISMATCH (-7)

#define SISY_INVALID_HANDLE UINT64_C(0)

#define SISY_CAP_LOG (UINT64_C(1) << 0u)
#define SISY_CAP_ALLOC (UINT64_C(1) << 1u)
#define SISY_CAP_CLOCK (UINT64_C(1) << 2u)
#define SISY_CAP_SLEEP (UINT64_C(1) << 3u)
#define SISY_CAP_MMIO (UINT64_C(1) << 4u)
#define SISY_CAP_DMA (UINT64_C(1) << 5u)
#define SISY_CAP_IRQ (UINT64_C(1) << 6u)
#define SISY_CAP_DEVICE_PUBLISH (UINT64_C(1) << 7u)

#define SISY_LOG_ERROR 1u
#define SISY_LOG_WARN 2u
#define SISY_LOG_INFO 3u
#define SISY_LOG_DEBUG 4u
#define SISY_LOG_TRACE 5u

#define SISY_BUS_PLATFORM 1u
#define SISY_BUS_PCI 2u
#define SISY_BUS_USB 3u
#define SISY_BUS_VIRTIO 4u

#if defined(__GNUC__) || defined(__clang__)
#define SISY_DRIVER_EXPORT __attribute__((visibility("default")))
#else
#define SISY_DRIVER_EXPORT
#endif

#define SISY_STRUCT_HAS(type, available_size, member)                         \
    ((available_size) >=                                                     \
     (offsetof(type, member) + sizeof(((type *)0)->member)))

typedef int32_t sisy_status_t;
typedef uint64_t sisy_handle_t;

struct sisy_kernel_api;
struct sisy_device_info;
struct sisy_driver_descriptor;

typedef void (*sisy_irq_handler_fn)(void *driver_context);

typedef sisy_status_t (*sisy_log_fn)(void *kernel_context, uint32_t level,
                                    const uint8_t *message,
                                    size_t message_len);
typedef sisy_status_t (*sisy_alloc_fn)(void *kernel_context, size_t size,
                                      size_t alignment, uint64_t flags,
                                      void **out_pointer);
typedef sisy_status_t (*sisy_dealloc_fn)(void *kernel_context, void *pointer,
                                        size_t size, size_t alignment);
typedef uint64_t (*sisy_monotonic_ns_fn)(void *kernel_context);
typedef sisy_status_t (*sisy_sleep_ns_fn)(void *kernel_context,
                                         uint64_t duration_ns);
typedef sisy_status_t (*sisy_mmio_map_fn)(void *kernel_context,
                                         uint64_t physical_address,
                                         size_t length, uint64_t flags,
                                         sisy_handle_t *out_handle,
                                         uint8_t **out_pointer);
typedef sisy_status_t (*sisy_mmio_unmap_fn)(void *kernel_context,
                                           sisy_handle_t mapping);
typedef sisy_status_t (*sisy_dma_alloc_fn)(void *kernel_context, size_t size,
                                          size_t alignment, uint64_t flags,
                                          sisy_handle_t *out_handle,
                                          void **out_cpu_pointer,
                                          uint64_t *out_device_address);
typedef sisy_status_t (*sisy_dma_free_fn)(void *kernel_context,
                                         sisy_handle_t allocation);
typedef sisy_status_t (*sisy_irq_register_fn)(
    void *kernel_context, uint32_t irq, uint64_t flags,
    sisy_irq_handler_fn handler, void *driver_context,
    sisy_handle_t *out_handle);
typedef sisy_status_t (*sisy_irq_set_enabled_fn)(void *kernel_context,
                                                sisy_handle_t registration,
                                                uint8_t enabled);
typedef sisy_status_t (*sisy_irq_unregister_fn)(void *kernel_context,
                                               sisy_handle_t registration);
typedef sisy_status_t (*sisy_device_publish_fn)(
    void *kernel_context, sisy_handle_t parent,
    const struct sisy_device_info *device, sisy_handle_t *out_handle);
typedef sisy_status_t (*sisy_device_remove_fn)(void *kernel_context,
                                              sisy_handle_t device);

struct sisy_kernel_api {
    uint32_t abi_version;
    uint32_t struct_size;
    uint64_t capabilities;
    void *kernel_context;
    sisy_log_fn log;
    sisy_alloc_fn alloc;
    sisy_dealloc_fn dealloc;
    sisy_monotonic_ns_fn monotonic_ns;
    sisy_sleep_ns_fn sleep_ns;
    sisy_mmio_map_fn mmio_map;
    sisy_mmio_unmap_fn mmio_unmap;
    sisy_dma_alloc_fn dma_alloc;
    sisy_dma_free_fn dma_free;
    sisy_irq_register_fn irq_register;
    sisy_irq_set_enabled_fn irq_set_enabled;
    sisy_irq_unregister_fn irq_unregister;
    sisy_device_publish_fn device_publish;
    sisy_device_remove_fn device_remove;
};

struct sisy_device_info {
    uint32_t struct_size;
    uint32_t bus_type;
    sisy_handle_t kernel_handle;
    uint32_t vendor_id;
    uint32_t device_id;
    uint32_t subsystem_vendor_id;
    uint32_t subsystem_device_id;
    uint32_t class_code;
    uint32_t revision;
    const uint8_t *address;
    size_t address_len;
};

typedef sisy_status_t (*sisy_probe_fn)(
    void *driver_context, const struct sisy_kernel_api *api,
    const struct sisy_device_info *device, void **out_instance);
typedef sisy_status_t (*sisy_remove_fn)(
    void *instance, const struct sisy_kernel_api *api,
    const struct sisy_device_info *device);
typedef sisy_status_t (*sisy_power_fn)(
    void *instance, const struct sisy_kernel_api *api);

struct sisy_driver_descriptor {
    uint32_t abi_version;
    uint32_t struct_size;
    uint64_t driver_version;
    uint64_t required_capabilities;
    const uint8_t *name;
    size_t name_len;
    void *driver_context;
    sisy_probe_fn probe;
    sisy_remove_fn remove;
    sisy_power_fn suspend;
    sisy_power_fn resume;
};

typedef sisy_status_t (*sisy_driver_entry_fn)(
    const struct sisy_kernel_api *api,
    struct sisy_driver_descriptor *out_driver, size_t out_driver_size);

SISY_DRIVER_EXPORT sisy_status_t sisyphus_driver_entry(
    const struct sisy_kernel_api *api,
    struct sisy_driver_descriptor *out_driver, size_t out_driver_size);

#ifdef __cplusplus
}
#endif

#endif
