idf_component_register(
    SRCS "main.c"
    INCLUDE_DIRS "."
    REQUIRES wren esp_timer driver esp_driver_usb_serial_jtag vfs
)
