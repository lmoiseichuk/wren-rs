# Build upstream Wren as an IDF component, from the submodule, unmodified.
#
# **Every path here points into `vendor/wren` and nothing is copied.** Copying
# would make the submodule decorative: the copy would drift, and the whole point
# is that this measures Wren as published rather than Wren as adapted.
set(WREN_ROOT ${CMAKE_CURRENT_LIST_DIR}/../../../../vendor/wren)

file(GLOB WREN_VM_SOURCES ${WREN_ROOT}/src/vm/*.c)
file(GLOB WREN_OPT_SOURCES ${WREN_ROOT}/src/optional/*.c)

idf_component_register(
    SRCS ${WREN_VM_SOURCES} ${WREN_OPT_SOURCES}
    INCLUDE_DIRS
        ${WREN_ROOT}/src/include
        ${WREN_ROOT}/src/vm
        ${WREN_ROOT}/src/optional
)

# **Upstream does not compile under IDF's warning settings, and that is a
# finding rather than a nuisance.** IDF builds with `-Werror=all -Wextra`; Wren
# is developed against looser ones. Two of them are errors here:
#
#   wren_compiler.c:4127  `%.*s` takes its precision as `int`, and the argument
#                         is `int32_t` -- which on this target is `long int`.
#                         A genuine portability bug, harmless on a platform
#                         where the two coincide and not on this one.
#
#   wren_compiler.c:1053  GCC 14 sees `parser.current` read on a path where it
#                         may not have been assigned.
#
# Demoted to warnings *for this component only*, because the submodule is not
# ours to edit -- see ../../README.md. Both are recorded under "How this
# differs" in this port's README, since each is a thing the Rust implementation
# should simply not have.
target_compile_options(${COMPONENT_LIB} PRIVATE
    -Wno-unused-parameter
    -Wno-sign-compare
    -Wno-implicit-fallthrough
    -Wno-error=format=
    -Wno-error=uninitialized
)
