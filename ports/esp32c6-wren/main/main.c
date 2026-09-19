// Upstream Wren on an ESP32-C6: a console, a benchmark runner, and a report of
// what it costs.
//
// **This is the control, not a product.** Everything here exists so the Rust
// implementation can be compared against the same language on the same silicon,
// running the same benchmarks, measured the same way. Nothing is tuned; tuning
// the reference would be tuning the thing we are measuring against.

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "esp_heap_caps.h"
#include "esp_system.h"
#include "esp_timer.h"
#include "freertos/FreeRTOS.h"
#include "freertos/task.h"
#include "driver/usb_serial_jtag.h"
#include "esp_vfs_usb_serial_jtag.h"
#include "esp_vfs_dev.h"

#include "wren.h"

// Upstream's foreign-function fixtures, so the `api/` group can run. Optional:
// see components/wren_api_tests/CMakeLists.txt.
#if __has_include("api_tests.h")
#include "api_tests.h"
#define HAVE_API_TESTS 1
#endif

// How much a line of input may be.
//
// **2 KB, because the test suite has lines longer than 512 bytes.** The
// `limit/` group generates deliberately enormous programs, and `long_function`
// puts a great deal on one line. A truncated line does not fail loudly: it
// usually still compiles and quietly means something else, which arrived as
// `unexpected compile error` on three tests that have nothing wrong with them.
//
// Static rather than heap so the heap numbers reported below are about Wren
// rather than about this file.
#define LINE_MAX 2048

// ---------------------------------------------------------------------------
// Wren's hooks into the outside world.
// ---------------------------------------------------------------------------

static void write_fn(WrenVM* vm, const char* text) {
    (void)vm;
    fputs(text, stdout);
}

static void error_fn(WrenVM* vm, WrenErrorType type, const char* module,
                     int line, const char* message) {
    (void)vm;
    switch (type) {
        case WREN_ERROR_COMPILE:
            printf("compile error: %s:%d: %s\n", module ? module : "?", line, message);
            break;
        case WREN_ERROR_RUNTIME:
            printf("runtime error: %s\n", message);
            break;
        case WREN_ERROR_STACK_TRACE:
            printf("  at %s:%d in %s\n", module ? module : "?", line, message);
            break;
    }
}

// ---------------------------------------------------------------------------
// Modules, for `import`.
// ---------------------------------------------------------------------------
//
// **There is no filesystem here, so `import` needs somewhere else to look.**
// Upstream's CLI resolves a module name to a path and reads the file; this
// board has neither. Instead the host registers a module's source over the
// console before running a test that imports it, and `loadModuleFn` serves from
// that registry.
//
// Without this, every one of the fourteen `language/module/*` tests fails with a
// runtime error -- which reads as "Wren's imports do not work on this part" and
// is really "this port never told Wren where to look".
//
// The name is whatever the program wrote: `import "./module"` arrives here as
// `./module`, because no `resolveModuleFn` is installed. That is deliberate --
// resolution is the embedder's policy, and for a test harness the literal
// string is the right key.
#define MODULE_MAX 8
#define MODULE_NAME_MAX 96

typedef struct {
    char name[MODULE_NAME_MAX];
    char* source;   // owned; freed when the slot is reused or cleared
} module_entry;

static module_entry modules[MODULE_MAX];
static int module_count = 0;

static void modules_clear(void) {
    for (int i = 0; i < module_count; i++) {
        free(modules[i].source);
        modules[i].source = NULL;
    }
    module_count = 0;
}

// Wren frees the source it was handed; this is how it says so.
static void load_module_complete(WrenVM* vm, const char* name,
                                 WrenLoadModuleResult result) {
    (void)vm;
    (void)name;
    free((void*)result.source);
}

static WrenLoadModuleResult load_module(WrenVM* vm, const char* name) {
    (void)vm;
    WrenLoadModuleResult result;
    result.source = NULL;
    result.onComplete = NULL;
    result.userData = NULL;

    for (int i = 0; i < module_count; i++) {
        if (strcmp(modules[i].name, name) != 0) continue;
        // **A copy, because Wren takes ownership.** Handing over the registry's
        // own pointer would have it freed underneath us and the module would be
        // gone for the next test that imports it.
        size_t length = strlen(modules[i].source);
        char* copy = (char*)malloc(length + 1);
        if (copy == NULL) return result;
        memcpy(copy, modules[i].source, length + 1);
        result.source = copy;
        result.onComplete = load_module_complete;
        return result;
    }
    // Not found: Wren reports "Could not load module", which is what the tests
    // for a missing import expect.
    return result;
}

// **Wren's default garbage collector thresholds cannot work on this part.**
//
// `wrenInitConfiguration` sets `initialHeapSize` to 10 MB and `minHeapSize` to
// 1 MB, and `vm->nextGC` starts at the former -- so the collector does not run
// until 10 MB has been allocated. This board has about 240 KB of heap free once
// the VM exists, so on the defaults the collector never runs *at all*:
// allocation keeps succeeding until it does not, and Wren stores through the
// returned NULL without checking. It arrives as a store access fault with
// `A0 = 0`, which reads like a Wren bug and is really a configuration one.
//
// These numbers are a port-level decision and not a modification of upstream --
// they are what `WrenConfiguration` is for. The Rust implementation should not
// need them: defaults that cannot work on the target are a thing to get right
// rather than to document.
#define WREN_INITIAL_HEAP (64 * 1024)
#define WREN_MIN_HEAP     (16 * 1024)

// One configuration, so the console VM and the benchmark VMs cannot drift apart
// and produce numbers that are not comparable.
static void configure(WrenConfiguration* config) {
    wrenInitConfiguration(config);
    config->writeFn = write_fn;
    config->errorFn = error_fn;
    config->initialHeapSize = WREN_INITIAL_HEAP;
    config->minHeapSize = WREN_MIN_HEAP;
    config->loadModuleFn = load_module;
#ifdef HAVE_API_TESTS
    // **Bound unconditionally, but they only fire for `./test/...` modules.**
    // `api_tests.c` gates on that prefix itself, so an ordinary program run as
    // `main` is unaffected and pays nothing for these being present.
    config->bindForeignMethodFn = APITest_bindForeignMethod;
    config->bindForeignClassFn = APITest_bindForeignClass;
#endif
}

// ---------------------------------------------------------------------------
// What this port owes the comparison: size, memory, and a benchmark runner.
// See ../README.md.
// ---------------------------------------------------------------------------

// Free heap, in bytes. **The number that matters most**: on a part where the
// whole budget is the point, a VM that does not leave the user program room is
// unusable whatever its speed.
static size_t free_heap(void) {
    return heap_caps_get_free_size(MALLOC_CAP_DEFAULT);
}

static void report_memory(const char* when, size_t baseline) {
    size_t now = free_heap();
    printf("mem:  %-14s free %7u B", when, (unsigned)now);
    if (baseline > 0 && baseline >= now) {
        printf("  (used %u B)", (unsigned)(baseline - now));
    }
    printf("\n");
}

// The shared benchmark set, as Wren source.
//
// **Kept in the port rather than loaded from a file** because there is no
// filesystem here and because the exact source has to travel with the number:
// a benchmark someone can read is a benchmark someone can check was the same in
// both languages. The Python translations live beside the MicroPython port.
typedef struct {
    const char* name;
    const char* source;
} benchmark;

static const benchmark BENCHMARKS[] = {
    // Method dispatch and arithmetic, which is what a scripting VM spends its
    // life doing.
    {"fib",
     "class Fib {\n"
     "  static of(n) { n < 2 ? n : of(n - 1) + of(n - 2) }\n"
     "}\n"
     "System.print(Fib.of(24))\n"},

    // Allocation and the collector, rather than arithmetic.
    {"tree",
     "class Tree {\n"
     "  construct new(depth) {\n"
     "    _depth = depth\n"
     "    if (depth > 0) {\n"
     "      _left = Tree.new(depth - 1)\n"
     "      _right = Tree.new(depth - 1)\n"
     "    }\n"
     "  }\n"
     "  sum { _depth == 0 ? 1 : 1 + _left.sum + _right.sum }\n"
     "}\n"
     "var total = 0\n"
     "for (i in 1..40) { total = total + Tree.new(10).sum }\n"
     "System.print(total)\n"},

    // A tight numeric loop, to isolate the interpreter's dispatch overhead from
    // everything else. On a part with no hardware double, this is also where
    // soft-float shows up.
    {"loop",
     "var x = 0\n"
     "for (i in 1..200000) { x = x + i % 7 }\n"
     "System.print(x)\n"},
};

static const int BENCHMARK_COUNT = sizeof(BENCHMARKS) / sizeof(BENCHMARKS[0]);

// Defined with the task below; a benchmark wants to report it per run.
static size_t stack_used(void);

// Run one benchmark in a fresh VM, and report time and peak cost.
//
// **A fresh VM per run**, so one benchmark's garbage cannot flatter or burden
// the next, and so the memory figure is what that benchmark alone needed.
static void run_benchmark(const benchmark* which) {
    size_t before = free_heap();

    WrenConfiguration config;
    configure(&config);

    WrenVM* vm = wrenNewVM(&config);
    size_t after_vm = free_heap();

    int64_t started = esp_timer_get_time();
    WrenInterpretResult result = wrenInterpret(vm, "main", which->source);
    int64_t elapsed = esp_timer_get_time() - started;

    size_t lowest = free_heap();
    wrenFreeVM(vm);
    size_t recovered = free_heap();

    // **Stack beside heap, per benchmark.** The two are separate budgets and a
    // program can be comfortable in one and out of the other -- `tree` is heap
    // hungry and shallow, a deep recursion is the reverse. Reporting only the
    // heap would hide half of what decides whether a part can run this.
    //
    // The figure is the task's high-water mark, which FreeRTOS tracks as the
    // minimum free ever seen and cannot be reset. So it is monotonic across a
    // run: what matters is *which* benchmark makes it grow.
    printf("bench: %-6s %8lld us   vm %5u B   peak %6u B   stack %5u B   %s\n",
           which->name,
           (long long)elapsed,
           (unsigned)(before - after_vm),
           (unsigned)(before - lowest),
           (unsigned)stack_used(),
           result == WREN_RESULT_SUCCESS ? "ok" : "FAILED");

    if (recovered + 64 < before) {
        // Worth saying rather than ignoring: a VM that does not give its memory
        // back is the failure mode that only shows up after hours of running.
        printf("bench: %-6s LEAKED %u B\n",
               which->name, (unsigned)(before - recovered));
    }
}

// ---------------------------------------------------------------------------
// Running a whole file, for the test suite.
// ---------------------------------------------------------------------------

// Defined below with the rest of the console; declared here because `.run`
// needs to keep reading lines of its own.
static int read_line(char* buffer, size_t limit);
static size_t stack_used(void);

// **Echo off while a file is being sent, and that is not cosmetic.**
//
// The console echoes each character so a person typing can see what they typed.
// During `.run` the sender is a script, which is not reading -- so the echo
// fills the device's transmit buffer, the write blocks, the reader stops
// draining, and the tail of the file is lost. The first test to expose this
// printed four of its twenty lines and looked like a Wren bug.
static int echo_enabled = 1;

// **The console evaluates one line at a time, and a test file is not one line.**
// `.run` collects lines until a lone `.end` and interprets the lot as a single
// module, which is what upstream's own runner does with a file. Without it every
// test with a class or an `if` block would fail for reasons that have nothing to
// do with Wren.
//
// **64 KB.** The `limit/` group is the reason: `many_constants` declares 65,536
// of them and `many_globals` 4,096, so those files run to tens of kilobytes.
// At 16 KB they came back as `source too large`, which is the harness's limit
// being reported as Wren's.
//
// It is affordable: the board has ~385 KB of heap free at boot and this is
// static. A file that still does not fit is reported rather than silently
// truncated, since a truncated program usually still compiles and then quietly
// means something else.
#define SOURCE_MAX (64 * 1024)
static char source_buffer[SOURCE_MAX];

// Markers the host watches for. **Printed either side of the program's own
// output**, so the runner can tell Wren's output from the console's without
// guessing, and can tell a finished run from a board that rebooted mid-test.
#define RUN_BEGIN "<<<wren-begin>>>"
#define RUN_END   "<<<wren-end>>>"

// Read lines into `source_buffer` until a lone `.end`. Shared by `.run` and
// `.module`, which differ only in what they do with the result.
//
// Returns the length, or -1 if it did not fit.
static long collect_source(char* line, size_t line_limit) {
    size_t used = 0;
    int overflowed = 0;

    echo_enabled = 0;
    while (1) {
        int length = read_line(line, line_limit);
        if (length < 0) continue;
        if (strcmp(line, ".end") == 0) break;
        // **A `.run` while collecting means the last one never finished.**
        //
        // If an `.end` is lost -- and it can be, when the device is busy
        // running a slow test and its receive buffer fills -- collection never
        // stops, and everything after it is swallowed as program text:
        // the next `.run`, then the next file. It surfaced as a five-line test
        // reporting `Error at 'run'` on line 215, which is unreadable as
        // anything but a Wren bug.
        //
        // Starting over is the right recovery. The host is telling us a new
        // program begins here, and whatever came before is already lost.
        if (strcmp(line, ".run") == 0) {
            used = 0;
            overflowed = 0;
            continue;
        }
        if (used + (size_t)length + 2 >= SOURCE_MAX) {
            overflowed = 1;
            continue;   // keep draining to `.end`, or the next command inherits it
        }
        memcpy(source_buffer + used, line, (size_t)length);
        used += (size_t)length;
        source_buffer[used++] = '\n';
    }
    source_buffer[used] = '\0';
    echo_enabled = 1;
    return overflowed ? -1 : (long)used;
}

// `.module <name>` then source then `.end`: register a module for `import`.
//
// **The name is copied before anything else happens**, because the caller
// passes a pointer *into* the console's line buffer and `collect_source` reuses
// that same buffer for every following line. Reading it afterwards gave the
// tail of the module's first line as its name -- `registered le = "the module"`
// -- so every import then failed with "could not load module", which looks like
// the loader not working and is really a dangling pointer.
static void register_module(const char* name, char* line, size_t line_limit) {
    char name_copy[MODULE_NAME_MAX];
    size_t name_length = strlen(name);
    if (name_length >= MODULE_NAME_MAX) {
        printf("module: name longer than %d bytes, truncated\n", MODULE_NAME_MAX - 1);
        name_length = MODULE_NAME_MAX - 1;
    }
    memcpy(name_copy, name, name_length);
    name_copy[name_length] = '\0';

    long used = collect_source(line, line_limit);
    if (used < 0) {
        printf("module: source too large for %d bytes\n", SOURCE_MAX);
        return;
    }
    if (module_count >= MODULE_MAX) {
        printf("module: no free slot (%d registered)\n", MODULE_MAX);
        return;
    }
    char* copy = (char*)malloc((size_t)used + 1);
    if (copy == NULL) {
        printf("module: out of memory\n");
        return;
    }
    memcpy(copy, source_buffer, (size_t)used + 1);
    memcpy(modules[module_count].name, name_copy, name_length + 1);
    modules[module_count].source = copy;
    module_count++;
    printf("module: registered %s (%ld B)\n", name_copy, used);
}

static void run_source(WrenVM* vm, char* line, size_t line_limit,
                       const char* module) {
    long used = collect_source(line, line_limit);
    int overflowed = used < 0;

    printf("%s\n", RUN_BEGIN);
    if (overflowed) {
        printf("source too large for %d bytes\n", SOURCE_MAX);
    } else {
        // **Measured around every run, not just around benchmarks.** Upstream's
        // benchmark files time themselves with `System.clock` and print their
        // own `elapsed:` line, which is what makes their numbers comparable
        // with published ones from other machines. What they cannot report is
        // what the run cost this board, so the port adds it: wall time, the
        // heap low-water mark, and the stack high-water mark.
        size_t before = free_heap();
        int64_t started = esp_timer_get_time();

        WrenInterpretResult result = wrenInterpret(vm, module, source_buffer);

        int64_t elapsed = esp_timer_get_time() - started;
        size_t lowest = free_heap();

        switch (result) {
            case WREN_RESULT_SUCCESS: break;
            case WREN_RESULT_COMPILE_ERROR: printf("[result] compile error\n"); break;
            case WREN_RESULT_RUNTIME_ERROR: printf("[result] runtime error\n"); break;
        }
#ifdef HAVE_API_TESTS
        // **Five of the `api/` tests are driven from C, not from Wren.**
        // The `.wren` half defines a class and the C half calls into it with
        // `wrenCall`; running only the script produces no output at all, which
        // is why they failed with nothing printed even once the foreign methods
        // bound. `APITest_Run` matches on the test name and is a no-op for
        // every other module, so this is safe to call unconditionally.
        if (result == WREN_RESULT_SUCCESS && strncmp(module, "./test/api/", 11) == 0) {
            char with_extension[MODULE_NAME_MAX + 8];
            snprintf(with_extension, sizeof(with_extension), "%s.wren", module);
            APITest_Run(vm, with_extension);
        }
#endif
        printf("[cost] %lld us  heap %u B  stack %u B\n",
               (long long)elapsed,
               (unsigned)(before > lowest ? before - lowest : 0),
               (unsigned)stack_used());
    }
    printf("%s\n", RUN_END);
}

// ---------------------------------------------------------------------------
// The console.
// ---------------------------------------------------------------------------

static void print_banner(void) {
    printf("\n=== esp32c6-wren ===\n");
    printf("wren:  %s (upstream, unmodified)\n", WREN_VERSION_STRING);
    printf("idf:   %s\n", esp_get_idf_version());
    printf("heap:  %u B free at boot\n", (unsigned)free_heap());
    // **The harness's own static cost, stated so it can be subtracted.** The
    // source and line buffers exist to carry the test suite over a serial line;
    // they are not Wren's and should not be charged to it when the Rust
    // implementation is compared against these numbers.
    printf("host:  %u B of that is this console's buffers\n",
           (unsigned)(SOURCE_MAX + LINE_MAX));
    printf("type wren source and press enter, or:\n");
    printf("  .run ... .end run a whole file\n");
    printf("  .module <name> ... .end  register a module for import\n");
    printf("  .modules      forget every registered module\n");
    printf("  .bench        run every benchmark\n");
    printf("  .mem          report free heap\n");
    printf("  .stack        report stack high-water mark\n");
    printf("  .help         this\n");
}

// Read one line from the console.
//
// Returns the length, or -1 if nothing arrived. Blocking with a short timeout
// rather than busy-waiting, so the idle task still runs.
static int read_line(char* buffer, size_t limit) {
    size_t length = 0;
    while (length + 1 < limit) {
        int c = fgetc(stdin);
        if (c == EOF) {
            vTaskDelay(pdMS_TO_TICKS(10));
            continue;
        }
        if (c == '\r' || c == '\n') {
            if (echo_enabled) fputc('\n', stdout);
            buffer[length] = '\0';
            return (int)length;
        }
        if ((c == 0x7f || c == '\b') && length > 0) {
            length--;
            if (echo_enabled) fputs("\b \b", stdout);
            continue;
        }
        buffer[length++] = (char)c;
        if (echo_enabled) fputc(c, stdout);
    }
    buffer[length] = '\0';
    return (int)length;
}

// **Wren runs in its own task, with a stack we chose and can measure.**
//
// The first attempt ran it on the main task with IDF's stack raised to 16 KB
// and it overflowed on `wrenNewVM` -- before any user code, while compiling
// Wren's own core library. That is worth knowing precisely rather than
// working around, so the stack is a named constant and the high-water mark is
// reported: "how much stack does this need" is a number the Rust version has
// to beat, and it cannot be beaten if it was never measured.
#define WREN_TASK_STACK 65536

// `uxTaskGetStackHighWaterMark` returns the *minimum free* the task has ever
// had, in words on this port. What was used is the rest.
static size_t stack_used(void) {
    UBaseType_t free_words = uxTaskGetStackHighWaterMark(NULL);
    return WREN_TASK_STACK - (size_t)free_words * sizeof(StackType_t);
}

static void report_stack(void) {
    size_t used = stack_used();
    printf("stack: %u B of %u used, %u B never touched\n",
           (unsigned)used,
           (unsigned)WREN_TASK_STACK,
           (unsigned)(WREN_TASK_STACK - used));
}

static void wren_task(void* arg) {
    // **Line endings, before anything reads.** IDF's VFS rewrites incoming
    // `\r\n` before a read returns, so collapsing them in the reader does
    // nothing -- the driver has to be told. The moisture project lost a day to
    // exactly this.
    //
    // **And the buffers, which default to 256 bytes each.** A test file arrives
    // as a burst of a few kilobytes; at 256 bytes the driver drops the tail
    // while the console is still reading character by character through the
    // VFS. Measured: four of twenty expected lines survived at full speed,
    // eleven at 5 ms per line, eighteen at 20 ms. That reads exactly like a
    // Wren bug and is a buffer size.
    usb_serial_jtag_driver_config_t usb_config = USB_SERIAL_JTAG_DRIVER_CONFIG_DEFAULT();
    usb_config.rx_buffer_size = 4096;
    usb_config.tx_buffer_size = 4096;
    usb_serial_jtag_driver_install(&usb_config);
    esp_vfs_usb_serial_jtag_use_driver();
    esp_vfs_dev_usb_serial_jtag_set_rx_line_endings(ESP_LINE_ENDINGS_LF);
    esp_vfs_dev_usb_serial_jtag_set_tx_line_endings(ESP_LINE_ENDINGS_CRLF);
    setvbuf(stdin, NULL, _IONBF, 0);
    setvbuf(stdout, NULL, _IONBF, 0);

    (void)arg;
    print_banner();

    // One long-lived VM for the interactive console, so that variables defined
    // on one line survive to the next. Benchmarks get their own.
    size_t before_vm = free_heap();
    WrenConfiguration config;
    configure(&config);
    WrenVM* vm = wrenNewVM(&config);
    report_memory("vm created", before_vm);
    // Straight after `wrenNewVM`, because compiling the core library is the
    // deepest this gets before any user code runs.
    report_stack();

    static char line[LINE_MAX];
    while (1) {
        fputs("> ", stdout);
        int length = read_line(line, sizeof(line));
        if (length <= 0) {
            continue;
        }

        if (strcmp(line, ".help") == 0) {
            print_banner();
        } else if (strcmp(line, ".mem") == 0) {
            report_memory("now", 0);
        } else if (strncmp(line, ".run", 4) == 0) {
            // **A fresh VM per test**, as upstream's runner gives each file its
            // own process: a test that defines a class must not collide with the
            // next one, and a test that corrupts the VM must not poison the run.
            // **The module name is copied before the buffer is reused**, for
            // the same reason `.module` copies its name: `collect_source` reads
            // into `line`, and a pointer into it would be reading the program's
            // own text by the time it was used.
            char module[MODULE_NAME_MAX] = "main";
            if (line[4] == ' ' && line[5] != '\0') {
                size_t length = strlen(line + 5);
                if (length >= MODULE_NAME_MAX) length = MODULE_NAME_MAX - 1;
                memcpy(module, line + 5, length);
                module[length] = '\0';
            }
            wrenFreeVM(vm);
            WrenConfiguration fresh;
            configure(&fresh);
            vm = wrenNewVM(&fresh);
            run_source(vm, line, sizeof(line), module);
        } else if (strncmp(line, ".module ", 8) == 0) {
            register_module(line + 8, line, sizeof(line));
        } else if (strcmp(line, ".modules") == 0) {
            modules_clear();
            printf("module: registry cleared\n");
        } else if (strcmp(line, ".stack") == 0) {
            report_stack();
        } else if (strcmp(line, ".bench") == 0) {
            for (int i = 0; i < BENCHMARK_COUNT; i++) {
                run_benchmark(&BENCHMARKS[i]);
            }
            report_stack();
        } else {
            wrenInterpret(vm, "main", line);
        }
    }
}

void app_main(void) {
    // A stack this size will not fit in IDF's main task, and raising *that*
    // would charge every other task for Wren's appetite.
    xTaskCreate(wren_task, "wren", WREN_TASK_STACK, NULL, 5, NULL);
}
