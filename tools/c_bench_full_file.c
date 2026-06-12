#include <errno.h>
#include <inttypes.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#ifdef _WIN32
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#else
#include <time.h>
#endif

#include "opl3.h"

#define CHIP_RATE 49716u
#define VGM_RATE 44100u
#define DEFAULT_IMF_RATE 700u

typedef enum op_kind {
    OP_WRITE,
    OP_SAMPLES
} op_kind;

typedef struct op {
    op_kind kind;
    uint16_t reg;
    uint8_t value;
    uint64_t samples;
} op;

typedef struct op_list {
    op *items;
    size_t len;
    size_t cap;
    uint64_t frames;
    uint64_t writes;
} op_list;

typedef enum file_format {
    FORMAT_AUTO,
    FORMAT_VGM,
    FORMAT_IMF
} file_format;

typedef struct options {
    const char *path;
    file_format format;
    int buffered;
    int buffered_set;
    uint32_t imf_rate;
    size_t iterations;
} options;

static void usage(const char *program)
{
    fprintf(stderr,
        "usage: %s [--vgm|--imf] [--buffered|--sync-writes] "
        "[--imf-rate HZ] [--iters N] FILE\n",
        program);
}

static void die(const char *message)
{
    fprintf(stderr, "error: %s\n", message);
    exit(1);
}

static void die_errno(const char *context, const char *path)
{
    fprintf(stderr, "error: %s %s: %s\n", context, path, strerror(errno));
    exit(1);
}

static uint16_t read_le16(const uint8_t *data, size_t offset)
{
    return (uint16_t)data[offset] | ((uint16_t)data[offset + 1] << 8);
}

static uint32_t read_le32(const uint8_t *data, size_t offset)
{
    return (uint32_t)data[offset] |
        ((uint32_t)data[offset + 1] << 8) |
        ((uint32_t)data[offset + 2] << 16) |
        ((uint32_t)data[offset + 3] << 24);
}

static int has_suffix_ci(const char *text, const char *suffix)
{
    size_t text_len = strlen(text);
    size_t suffix_len = strlen(suffix);
    size_t i;

    if (suffix_len > text_len) {
        return 0;
    }
    text += text_len - suffix_len;
    for (i = 0; i < suffix_len; i++) {
        char a = text[i];
        char b = suffix[i];
        if (a >= 'A' && a <= 'Z') {
            a = (char)(a - 'A' + 'a');
        }
        if (b >= 'A' && b <= 'Z') {
            b = (char)(b - 'A' + 'a');
        }
        if (a != b) {
            return 0;
        }
    }
    return 1;
}

static uint64_t parse_u64_arg(const char *name, const char *value)
{
    char *end = NULL;
    uint64_t parsed;

    if (value == NULL || *value == '\0') {
        fprintf(stderr, "error: %s requires a value\n", name);
        exit(1);
    }
    errno = 0;
    parsed = (uint64_t)strtoull(value, &end, 10);
    if (errno != 0 || end == value || *end != '\0') {
        fprintf(stderr, "error: %s must be an unsigned integer\n", name);
        exit(1);
    }
    return parsed;
}

static void parse_options(options *opts, int argc, char **argv)
{
    int i;
    const char *env_iters;

    memset(opts, 0, sizeof(*opts));
    opts->format = FORMAT_AUTO;
    opts->imf_rate = DEFAULT_IMF_RATE;
    opts->iterations = 1;

    env_iters = getenv("OPL3_RS_BENCH_ITERS");
    if (env_iters != NULL && *env_iters != '\0') {
        opts->iterations = (size_t)parse_u64_arg("OPL3_RS_BENCH_ITERS", env_iters);
    }

    for (i = 1; i < argc; i++) {
        const char *arg = argv[i];
        if (strcmp(arg, "--help") == 0 || strcmp(arg, "-h") == 0) {
            usage(argv[0]);
            exit(0);
        } else if (strcmp(arg, "--vgm") == 0) {
            opts->format = FORMAT_VGM;
        } else if (strcmp(arg, "--imf") == 0) {
            opts->format = FORMAT_IMF;
        } else if (strcmp(arg, "--buffered") == 0 || strcmp(arg, "-b") == 0) {
            opts->buffered = 1;
            opts->buffered_set = 1;
        } else if (strcmp(arg, "--sync-writes") == 0 ||
            strcmp(arg, "--unbuffered") == 0 || strcmp(arg, "-u") == 0) {
            opts->buffered = 0;
            opts->buffered_set = 1;
        } else if (strcmp(arg, "--imf-rate") == 0) {
            if (++i >= argc) {
                die("--imf-rate requires a value");
            }
            opts->imf_rate = (uint32_t)parse_u64_arg("--imf-rate", argv[i]);
        } else if (strncmp(arg, "--imf-rate=", 11) == 0) {
            opts->imf_rate = (uint32_t)parse_u64_arg("--imf-rate", arg + 11);
        } else if (strcmp(arg, "--iters") == 0) {
            if (++i >= argc) {
                die("--iters requires a value");
            }
            opts->iterations = (size_t)parse_u64_arg("--iters", argv[i]);
        } else if (strncmp(arg, "--iters=", 8) == 0) {
            opts->iterations = (size_t)parse_u64_arg("--iters", arg + 8);
        } else if (arg[0] == '-') {
            fprintf(stderr, "error: unknown option %s\n", arg);
            exit(1);
        } else if (opts->path == NULL) {
            opts->path = arg;
        } else {
            fprintf(stderr, "error: multiple input files: %s and %s\n", opts->path, arg);
            exit(1);
        }
    }

    if (opts->path == NULL) {
        usage(argv[0]);
        exit(1);
    }
    if (opts->iterations == 0) {
        die("--iters must be greater than zero");
    }
    if (opts->imf_rate == 0) {
        die("--imf-rate must be greater than zero");
    }
    if (opts->format == FORMAT_AUTO) {
        if (has_suffix_ci(opts->path, ".vgm")) {
            opts->format = FORMAT_VGM;
        } else if (has_suffix_ci(opts->path, ".imf")) {
            opts->format = FORMAT_IMF;
        } else {
            die("could not infer file format; pass --vgm or --imf");
        }
    }
    if (!opts->buffered_set) {
        opts->buffered = opts->format == FORMAT_VGM;
    }
}

static uint8_t *read_file(const char *path, size_t *len_out)
{
    FILE *file = fopen(path, "rb");
    long len;
    uint8_t *data;

    if (file == NULL) {
        die_errno("opening", path);
    }
    if (fseek(file, 0, SEEK_END) != 0) {
        die_errno("seeking", path);
    }
    len = ftell(file);
    if (len < 0) {
        die_errno("sizing", path);
    }
    if (fseek(file, 0, SEEK_SET) != 0) {
        die_errno("seeking", path);
    }

    data = (uint8_t *)malloc((size_t)len + 1);
    if (data == NULL) {
        die("out of memory reading input");
    }
    if (len > 0 && fread(data, 1, (size_t)len, file) != (size_t)len) {
        die_errno("reading", path);
    }
    if (fclose(file) != 0) {
        die_errno("closing", path);
    }
    *len_out = (size_t)len;
    return data;
}

static void op_list_free(op_list *ops)
{
    free(ops->items);
    memset(ops, 0, sizeof(*ops));
}

static void op_list_push(op_list *ops, op item)
{
    if (ops->len == ops->cap) {
        size_t new_cap = ops->cap == 0 ? 1024 : ops->cap * 2;
        op *new_items = (op *)realloc(ops->items, new_cap * sizeof(*new_items));
        if (new_items == NULL) {
            die("out of memory growing op list");
        }
        ops->items = new_items;
        ops->cap = new_cap;
    }
    ops->items[ops->len++] = item;
    if (item.kind == OP_WRITE) {
        ops->writes++;
    } else {
        ops->frames += item.samples;
    }
}

static void push_write(op_list *ops, uint16_t reg, uint8_t value)
{
    op item;
    item.kind = OP_WRITE;
    item.reg = reg;
    item.value = value;
    item.samples = 0;
    op_list_push(ops, item);
}

static void push_samples(op_list *ops, uint64_t samples)
{
    op item;
    if (samples == 0) {
        return;
    }
    item.kind = OP_SAMPLES;
    item.reg = 0;
    item.value = 0;
    item.samples = samples;
    op_list_push(ops, item);
}

static void ensure_range(size_t len, size_t offset, size_t count, const char *context)
{
    if (offset > len || count > len - offset) {
        fprintf(stderr, "error: truncated %s at offset 0x%zx\n", context, offset);
        exit(1);
    }
}

static void flush_vgm_wait(op_list *ops, uint64_t *pending_wait_44k, uint64_t *remainder)
{
    uint64_t numerator;
    uint64_t chip_samples;

    if (*pending_wait_44k == 0) {
        return;
    }
    numerator = (*pending_wait_44k * CHIP_RATE) + *remainder;
    chip_samples = numerator / VGM_RATE;
    *remainder = numerator % VGM_RATE;
    *pending_wait_44k = 0;
    push_samples(ops, chip_samples);
}

static void parse_vgm(const uint8_t *data, size_t len, op_list *ops)
{
    uint32_t version;
    size_t eof;
    size_t gd3;
    size_t data_offset;
    size_t end;
    size_t idx;
    uint64_t pending_wait_44k = 0;
    uint64_t remainder = 0;

    if (len < 0x40 || memcmp(data, "Vgm ", 4) != 0) {
        die("not a VGM file");
    }

    ensure_range(len, 0x34, 4, "VGM header");
    version = read_le32(data, 0x08);
    eof = (size_t)read_le32(data, 0x04) + 4u;
    gd3 = read_le32(data, 0x14) == 0 ? eof : (size_t)read_le32(data, 0x14) + 0x14u;
    if (version < 0x150) {
        data_offset = 0x40;
    } else {
        uint32_t offset = read_le32(data, 0x34);
        data_offset = offset == 0 ? 0x40 : (size_t)offset + 0x34u;
    }
    end = eof;
    if (gd3 < end) {
        end = gd3;
    }
    if (len < end) {
        end = len;
    }
    if (data_offset >= end) {
        die("VGM data offset is past end of data");
    }

    idx = data_offset;
    while (idx < end) {
        size_t command_offset = idx;
        uint8_t command = data[idx++];

        switch (command) {
        case 0x66:
            flush_vgm_wait(ops, &pending_wait_44k, &remainder);
            return;
        case 0x61:
            ensure_range(len, idx, 2, "VGM wait command");
            pending_wait_44k += read_le16(data, idx);
            idx += 2;
            break;
        case 0x62:
            pending_wait_44k += 735;
            break;
        case 0x63:
            pending_wait_44k += 882;
            break;
        case 0x5a:
        case 0x5e:
            ensure_range(len, idx, 2, "VGM OPL write");
            flush_vgm_wait(ops, &pending_wait_44k, &remainder);
            push_write(ops, data[idx], data[idx + 1]);
            idx += 2;
            break;
        case 0x5f:
            ensure_range(len, idx, 2, "VGM OPL write");
            flush_vgm_wait(ops, &pending_wait_44k, &remainder);
            push_write(ops, (uint16_t)data[idx] | 0x100u, data[idx + 1]);
            idx += 2;
            break;
        case 0x67: {
            uint32_t size;
            ensure_range(len, idx, 6, "VGM data block");
            if (data[idx] != 0x66) {
                fprintf(stderr, "error: malformed VGM data block at 0x%zx\n", command_offset);
                exit(1);
            }
            size = read_le32(data, idx + 2);
            idx += 6;
            ensure_range(len, idx, size, "VGM data block payload");
            idx += size;
            break;
        }
        case 0x68:
            ensure_range(len, idx, 11, "VGM PCM RAM write");
            idx += 11;
            break;
        case 0x30:
        case 0x31:
        case 0x4f:
        case 0x50:
            ensure_range(len, idx, 1, "VGM one-byte command");
            idx += 1;
            break;
        case 0x90:
        case 0x91:
        case 0x95:
            ensure_range(len, idx, 4, "VGM stream command");
            idx += 4;
            break;
        case 0x92:
            ensure_range(len, idx, 5, "VGM stream command");
            idx += 5;
            break;
        case 0x93:
            ensure_range(len, idx, 10, "VGM stream command");
            idx += 10;
            break;
        case 0x94:
            ensure_range(len, idx, 1, "VGM stream command");
            idx += 1;
            break;
        default:
            if (command >= 0x70 && command <= 0x7f) {
                pending_wait_44k += (uint64_t)(command & 0x0f) + 1u;
            } else if (command >= 0x80 && command <= 0x8f) {
                pending_wait_44k += (uint64_t)(command & 0x0f);
            } else if ((command >= 0x51 && command <= 0x5d) ||
                (command >= 0xa0 && command <= 0xbf)) {
                ensure_range(len, idx, 2, "VGM two-byte command");
                idx += 2;
            } else if (command >= 0xc0 && command <= 0xdf) {
                ensure_range(len, idx, 3, "VGM three-byte command");
                idx += 3;
            } else if (command >= 0xe0 && command <= 0xff) {
                ensure_range(len, idx, 4, "VGM four-byte command");
                idx += 4;
            } else {
                fprintf(stderr, "error: unsupported VGM command 0x%02x at 0x%zx\n",
                    command, command_offset);
                exit(1);
            }
            break;
        }
    }

    flush_vgm_wait(ops, &pending_wait_44k, &remainder);
}

static void parse_imf(const uint8_t *data, size_t len, uint32_t imf_rate, op_list *ops)
{
    size_t offset = 0;
    uint64_t remainder = 0;

    if (len >= 6) {
        uint16_t declared = read_le16(data, 0);
        if ((size_t)declared == len - 2 && declared % 4 == 0) {
            offset = 2;
        }
    }
    if (((len - offset) % 4) != 0) {
        die("IMF body length is not a multiple of 4 bytes");
    }

    while (offset < len) {
        uint16_t delay;
        uint64_t numerator;
        uint64_t chip_samples;

        push_write(ops, data[offset], data[offset + 1]);
        delay = read_le16(data, offset + 2);
        offset += 4;
        if (delay == 0) {
            continue;
        }
        numerator = ((uint64_t)delay * CHIP_RATE) + remainder;
        chip_samples = numerator / imf_rate;
        remainder = numerator % imf_rate;
        push_samples(ops, chip_samples);
    }
}

static uint64_t checksum_sample(uint64_t checksum, const int16_t sample[2])
{
    uint64_t left = (uint16_t)sample[0];
    uint64_t right = (uint16_t)sample[1];
    return (checksum * UINT64_C(0x100000001b3)) + (left | (right << 16));
}

static uint64_t render_trace(const op_list *ops, int buffered)
{
    opl3_chip chip;
    int16_t sample[2];
    uint64_t checksum = UINT64_C(0xcbf29ce484222325);
    size_t op_idx;

    OPL3_Reset(&chip, CHIP_RATE);
    for (op_idx = 0; op_idx < ops->len; op_idx++) {
        const op *item = &ops->items[op_idx];
        if (item->kind == OP_WRITE) {
            if (buffered) {
                OPL3_WriteRegBuffered(&chip, item->reg, item->value);
            } else {
                OPL3_WriteReg(&chip, item->reg, item->value);
            }
        } else {
            uint64_t i;
            for (i = 0; i < item->samples; i++) {
                OPL3_Generate(&chip, sample);
                checksum = checksum_sample(checksum, sample);
            }
        }
    }
    return checksum;
}

static double now_seconds(void)
{
#ifdef _WIN32
    static LARGE_INTEGER frequency;
    LARGE_INTEGER counter;
    if (frequency.QuadPart == 0) {
        QueryPerformanceFrequency(&frequency);
    }
    QueryPerformanceCounter(&counter);
    return (double)counter.QuadPart / (double)frequency.QuadPart;
#else
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (double)ts.tv_sec + (double)ts.tv_nsec / 1000000000.0;
#endif
}

int main(int argc, char **argv)
{
    options opts;
    uint8_t *data;
    size_t data_len;
    op_list ops;
    uint64_t checksum;
    uint64_t timed_checksum = 0;
    double start;
    double elapsed;
    double ns_frame;
    double duration_s;
    size_t i;
    const char *format_name;
    const char *mode_name;

    parse_options(&opts, argc, argv);
    data = read_file(opts.path, &data_len);
    memset(&ops, 0, sizeof(ops));

    if (opts.format == FORMAT_VGM) {
        parse_vgm(data, data_len, &ops);
        format_name = "vgm";
    } else {
        parse_imf(data, data_len, opts.imf_rate, &ops);
        format_name = "imf";
    }
    free(data);

    mode_name = opts.buffered ? "buffered" : "sync";
    duration_s = (double)ops.frames / (double)CHIP_RATE;
    checksum = render_trace(&ops, opts.buffered);

    start = now_seconds();
    for (i = 0; i < opts.iterations; i++) {
        timed_checksum ^= render_trace(&ops, opts.buffered);
    }
    elapsed = now_seconds() - start;
    if (timed_checksum != (opts.iterations % 2 == 0 ? 0 : checksum)) {
        fprintf(stderr, "error: timed checksum mismatch: 0x%016" PRIx64 "\n", timed_checksum);
        op_list_free(&ops);
        return 1;
    }

    ns_frame = elapsed * 1000000000.0 / ((double)ops.frames * (double)opts.iterations);
    printf("%s_%s_full_c: iterations=%zu, writes=%" PRIu64
           ", frames=%" PRIu64 ", duration=%.2fs, checksum=0x%016" PRIx64 "\n",
        format_name, mode_name, opts.iterations, ops.writes, ops.frames, duration_s, checksum);
    printf("%s_%s_full_c: c=%0.9fs (%0.2f ns/frame)\n",
        format_name, mode_name, elapsed, ns_frame);

    op_list_free(&ops);
    return 0;
}
