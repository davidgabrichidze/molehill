/*
 * libopenconnect's progress callback is variadic:
 *     void (*)(void *privdata, int level, const char *fmt, ...)
 * Stable Rust can't define a variadic function, and on Apple Silicon
 * variadic arguments travel on the stack, unlike fixed ones, so declaring
 * the callback with fixed arguments (what works on x64 Windows) would read
 * garbage. This shim is the real variadic callback: it formats the message
 * and hands Rust a finished line. macOS only.
 */
#include <stdarg.h>
#include <stdio.h>

typedef void (*molehill_line_fn)(void *privdata, int level, const char *line);

static molehill_line_fn line_fn;

void molehill_set_progress_line(molehill_line_fn fn) {
    line_fn = fn;
}

void molehill_progress(void *privdata, int level, const char *fmt, ...) {
    char buf[2048];
    va_list ap;
    if (!line_fn || !fmt)
        return;
    va_start(ap, fmt);
    vsnprintf(buf, sizeof buf, fmt, ap);
    va_end(ap);
    line_fn(privdata, level, buf);
}
