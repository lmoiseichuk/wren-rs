idf_component_register(
    SRCS "main.c"
    INCLUDE_DIRS "."
    REQUIRES wren wren_api_tests esp_timer driver esp_driver_usb_serial_jtag vfs
)
