#include <sisyphus/driver.h>

static const uint8_t driver_name[] = "sisyphus-reference";
static const uint8_t probe_message[] = "reference driver probe";
static const uint8_t child_address[] = "platform:reference-child";
static uint8_t instance_token;

static void reference_irq(void *driver_context) {
    (void)driver_context;
}

static sisy_status_t reference_probe(
    void *driver_context, const struct sisy_kernel_api *api,
    const struct sisy_device_info *device, void **out_instance) {
    (void)driver_context;

    if (api == NULL || device == NULL || out_instance == NULL) {
        return SISY_STATUS_INVALID_ARGUMENT;
    }
    if ((api->capabilities & SISY_CAP_LOG) == 0u || api->log == NULL) {
        return SISY_STATUS_UNSUPPORTED;
    }

    sisy_status_t status = api->log(api->kernel_context, SISY_LOG_INFO,
                                    probe_message,
                                    sizeof(probe_message) - 1u);
    if (status != SISY_STATUS_OK) {
        return status;
    }

    *out_instance = &instance_token;
    return SISY_STATUS_OK;
}

static sisy_status_t reference_remove(
    void *instance, const struct sisy_kernel_api *api,
    const struct sisy_device_info *device) {
    (void)api;
    (void)device;
    return instance == &instance_token ? SISY_STATUS_OK
                                       : SISY_STATUS_INVALID_ARGUMENT;
}

SISY_DRIVER_EXPORT sisy_status_t sisyphus_driver_entry(
    const struct sisy_kernel_api *api,
    struct sisy_driver_descriptor *out_driver, size_t out_driver_size) {
    if (api == NULL || out_driver == NULL) {
        return SISY_STATUS_INVALID_ARGUMENT;
    }
    if ((api->abi_version >> 16u) != SISY_ABI_MAJOR) {
        return SISY_STATUS_ABI_MISMATCH;
    }
    if (!SISY_STRUCT_HAS(struct sisy_kernel_api, api->struct_size, log)) {
        return SISY_STATUS_ABI_MISMATCH;
    }
    if (out_driver_size < sizeof(struct sisy_driver_descriptor)) {
        return SISY_STATUS_ABI_MISMATCH;
    }

    *out_driver = (struct sisy_driver_descriptor){
        .abi_version = SISY_ABI_VERSION,
        .struct_size = sizeof(struct sisy_driver_descriptor),
        .driver_version = UINT64_C(0x0001000000000000),
        .required_capabilities = SISY_CAP_LOG,
        .name = driver_name,
        .name_len = sizeof(driver_name) - 1u,
        .driver_context = NULL,
        .probe = reference_probe,
        .remove = reference_remove,
        .suspend = NULL,
        .resume = NULL,
    };
    return SISY_STATUS_OK;
}

size_t sisyphus_reference_sizeof_kernel_api(void) {
    return sizeof(struct sisy_kernel_api);
}

size_t sisyphus_reference_sizeof_device_info(void) {
    return sizeof(struct sisy_device_info);
}

size_t sisyphus_reference_sizeof_driver_descriptor(void) {
    return sizeof(struct sisy_driver_descriptor);
}

sisy_status_t sisyphus_reference_exercise_api(
    const struct sisy_kernel_api *api) {
    const uint64_t required = SISY_CAP_LOG | SISY_CAP_ALLOC | SISY_CAP_CLOCK |
                              SISY_CAP_SLEEP | SISY_CAP_MMIO | SISY_CAP_DMA |
                              SISY_CAP_IRQ | SISY_CAP_DEVICE_PUBLISH;
    void *allocation = NULL;
    sisy_handle_t mmio_handle = SISY_INVALID_HANDLE;
    uint8_t *mmio_pointer = NULL;
    sisy_handle_t dma_handle = SISY_INVALID_HANDLE;
    void *dma_pointer = NULL;
    uint64_t dma_address = 0;
    sisy_handle_t irq_handle = SISY_INVALID_HANDLE;
    sisy_handle_t child_handle = SISY_INVALID_HANDLE;

    if (api == NULL || (api->capabilities & required) != required) {
        return SISY_STATUS_UNSUPPORTED;
    }
    if (api->log == NULL || api->alloc == NULL || api->dealloc == NULL ||
        api->monotonic_ns == NULL || api->sleep_ns == NULL ||
        api->mmio_map == NULL || api->mmio_unmap == NULL ||
        api->dma_alloc == NULL || api->dma_free == NULL ||
        api->irq_register == NULL || api->irq_set_enabled == NULL ||
        api->irq_unregister == NULL || api->device_publish == NULL ||
        api->device_remove == NULL) {
        return SISY_STATUS_ABI_MISMATCH;
    }

    sisy_status_t status = api->log(api->kernel_context, SISY_LOG_DEBUG,
                                    probe_message,
                                    sizeof(probe_message) - 1u);
    if (status != SISY_STATUS_OK) {
        return status;
    }
    status = api->alloc(api->kernel_context, 64u, 16u, 0u, &allocation);
    if (status != SISY_STATUS_OK || allocation == NULL) {
        return status == SISY_STATUS_OK ? SISY_STATUS_NO_MEMORY : status;
    }
    status = api->dealloc(api->kernel_context, allocation, 64u, 16u);
    if (status != SISY_STATUS_OK) {
        return status;
    }

    (void)api->monotonic_ns(api->kernel_context);
    status = api->sleep_ns(api->kernel_context, UINT64_C(1000));
    if (status != SISY_STATUS_OK) {
        return status;
    }

    status = api->mmio_map(api->kernel_context, UINT64_C(0x1000), 32u, 0u,
                           &mmio_handle, &mmio_pointer);
    if (status != SISY_STATUS_OK || mmio_pointer == NULL) {
        return status == SISY_STATUS_OK ? SISY_STATUS_IO_ERROR : status;
    }
    status = api->mmio_unmap(api->kernel_context, mmio_handle);
    if (status != SISY_STATUS_OK) {
        return status;
    }

    status = api->dma_alloc(api->kernel_context, 32u, 16u, 0u, &dma_handle,
                            &dma_pointer, &dma_address);
    if (status != SISY_STATUS_OK || dma_pointer == NULL || dma_address == 0u) {
        return status == SISY_STATUS_OK ? SISY_STATUS_NO_MEMORY : status;
    }
    status = api->dma_free(api->kernel_context, dma_handle);
    if (status != SISY_STATUS_OK) {
        return status;
    }

    status = api->irq_register(api->kernel_context, 5u, 0u, reference_irq,
                               NULL, &irq_handle);
    if (status != SISY_STATUS_OK) {
        return status;
    }
    status = api->irq_set_enabled(api->kernel_context, irq_handle, 1u);
    if (status != SISY_STATUS_OK) {
        return status;
    }
    status = api->irq_set_enabled(api->kernel_context, irq_handle, 0u);
    if (status != SISY_STATUS_OK) {
        return status;
    }
    status = api->irq_unregister(api->kernel_context, irq_handle);
    if (status != SISY_STATUS_OK) {
        return status;
    }

    const struct sisy_device_info child = {
        .struct_size = sizeof(struct sisy_device_info),
        .bus_type = SISY_BUS_PLATFORM,
        .kernel_handle = SISY_INVALID_HANDLE,
        .vendor_id = 0u,
        .device_id = 0u,
        .subsystem_vendor_id = 0u,
        .subsystem_device_id = 0u,
        .class_code = 0u,
        .revision = 0u,
        .address = child_address,
        .address_len = sizeof(child_address) - 1u,
    };
    status = api->device_publish(api->kernel_context, SISY_INVALID_HANDLE,
                                 &child, &child_handle);
    if (status != SISY_STATUS_OK) {
        return status;
    }
    return api->device_remove(api->kernel_context, child_handle);
}
