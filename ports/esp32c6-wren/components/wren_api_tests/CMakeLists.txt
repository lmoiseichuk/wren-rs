# Upstream's foreign-function test harness, so the `api/` group can run.
#
# **Optional, because it is not free.** These are test fixtures, not part of
# Wren, and image size is one of the things this port exists to measure -- so
# carrying them in a build whose footprint gets published would be reporting the
# harness as the VM. `-DWREN_API_TESTS=OFF` leaves them out entirely.
#
# The dispatcher only binds when the module name begins with `./test/`
# (`api_tests.c:9`), which is why the console's `.run` takes an optional module
# name: a program run as `main` gets no foreign methods however many are
# compiled in.
if(NOT DEFINED WREN_API_TESTS OR WREN_API_TESTS)
    set(WREN_ROOT ${CMAKE_CURRENT_LIST_DIR}/../../../../vendor/wren)
    file(GLOB API_SOURCES ${WREN_ROOT}/test/api/*.c)

    idf_component_register(
        SRCS ${API_SOURCES}
        INCLUDE_DIRS ${WREN_ROOT}/test/api
        REQUIRES wren
    )
    # Upstream's test code, not ours to tidy -- see components/wren.
    target_compile_options(${COMPONENT_LIB} PRIVATE
        -Wno-unused-parameter
        -Wno-sign-compare
        -Wno-error=format=
        -Wno-error=uninitialized
        -Wno-error=stringop-truncation
        -Wno-error=stringop-overflow
        -Wno-error=unused-variable
    )
else()
    idf_component_register()
endif()
