// Upstream Wren on an ESP32-C6: a console, a benchmark runner, and a report of
// what it costs.
//
// **This is the control, not a product.** Everything here exists so the Rust
// implementation can be compared against the same language on the same silicon,
// running the same benchmarks, measured the same way. Nothing is tuned; tuning
// the reference would be tuning the thing we are measuring against.

#include <stdio.h>
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

// How much a line of input may be. Generous for a console, and it is static
// rather than heap so that the heap numbers printed below are about Wren rather
// than about this file.
#define LINE_MAX 512

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

    printf("bench: %-6s %8lld us   vm %5u B   peak %6u B   %s\n",
           which->name,
           (long long)elapsed,
           (unsigned)(before - after_vm),
           (unsigned)(before - lowest),
           result == WREN_RESULT_SUCCESS ? "ok" : "FAILED");

    if (recovered + 64 < before) {
        // Worth saying rather than ignoring: a VM that does not give its memory
        // back is the failure mode that only shows up after hours of running.
        printf("bench: %-6s LEAKED %u B\n",
               which->name, (unsigned)(before - recovered));
    }
}

// ---------------------------------------------------------------------------
// The console.
// ---------------------------------------------------------------------------

static void print_banner(void) {
    printf("\n=== esp32c6-wren ===\n");
    printf("wren:  %s (upstream, unmodified)\n", WREN_VERSION_STRING);
    printf("idf:   %s\n", esp_get_idf_version());
    printf("heap:  %u B free at boot\n", (unsigned)free_heap());
    printf("type wren source and press enter, or:\n");
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
            fputc('\n', stdout);
            buffer[length] = '\0';
            return (int)length;
        }
        if ((c == 0x7f || c == '\b') && length > 0) {
            length--;
            fputs("\b \b", stdout);
            continue;
        }
        buffer[length++] = (char)c;
        fputc(c, stdout);
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

static void report_stack(void) {
    // `uxTaskGetStackHighWaterMark` returns the *minimum free* the task has
    // ever had, in words on this port. The used figure is what is left over.
    UBaseType_t free_words = uxTaskGetStackHighWaterMark(NULL);
    size_t free_bytes = (size_t)free_words * sizeof(StackType_t);
    printf("stack: %u B of %u used, %u B never touched\n",
           (unsigned)(WREN_TASK_STACK - free_bytes),
           (unsigned)WREN_TASK_STACK,
           (unsigned)free_bytes);
}

static void wren_task(void* arg) {
    // **Line endings, before anything reads.** IDF's VFS rewrites incoming
    // `\r\n` before a read returns, so collapsing them in the reader does
    // nothing -- the driver has to be told. The moisture project lost a day to
    // exactly this.
    usb_serial_jtag_driver_config_t usb_config = USB_SERIAL_JTAG_DRIVER_CONFIG_DEFAULT();
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
