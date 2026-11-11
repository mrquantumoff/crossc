/* ssc_oracle: command-line front end over the BSD-3-Clause C reference encoder (samsung-ssc-encoder, src/)
 * and its bitstream-checker decoder (tests/decoder/), used only by the ssc-enc test suite.
 *
 * The encoder and decoder sources are NOT part of this crate; build_oracle.cmd compiles them from a checkout of
 * samsung-ssc-encoder (Copyright (c) 2026 William Ho / WillyBilly06, BSD-3-Clause; decoder parts adapted from
 * libopus 1.1, Copyright Xiph.Org Foundation and contributors, BSD-3-Clause). See ../NOTICE. read_file() and the
 * bitrate-list parsing are adapted from samsung-ssc-encoder tests/ssc_conformance.c (same copyright and license).
 *
 *   ssc_oracle enc -r RATE -fmt s16|s24|f32|f32raw|s32raw [-bits B] [-mono] [-flags N] -b BITRATE[,BITRATE...]
 *                  in.s16le out.ssc info.txt
 *       Input is raw s16le (except for f32raw and s32raw) (stereo interleaved, or mono with -mono, duplicated to both channels).
 *       s16 -> ssc_encode, s24 -> ssc_encode_s32(x * 256, 24 bits), f32 -> ssc_encode_float(x / 32768).
 *   ssc_oracle dec -r RATE [-pcm out.f32le] in.ssc
 *       Decodes with the checker decoder; prints "ret tell" per frame, optionally writes f32le stereo PCM.
 *       -fmt f32raw instead reads raw f32le stereo input and passes it to ssc_encode_float unchanged.
 *       -fmt s32raw [-bits B] instead reads raw s32le input (stereo, or mono with -mono, duplicated to both
 *       channels) and passes it to ssc_encode_s32(x, B) unchanged (B = 16..32, default 24).
 *       Frame k uses bitrate entry k mod n. Output: [u16 LE length][frame] per frame. info.txt: one line per
 *       frame with the ssc_encoder_frame_info fields.
 *   ssc_oracle dec -r RATE in.ssc
 *       Decodes every frame; prints "ret tell" per frame (tell = decoder's final ec_tell, SSC_DEBUG_TELL).
 *   ssc_oracle tables
 *       Prints the bit patterns of the float tables (window, eMeans) for a bit-exactness check. */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdint.h>

#include "ssc_encoder.h"
#include "ssc_decoder.h"

extern int ssc_debug_tell;
extern const float sscenc_window[108];
extern const float sscenc_eMeans[25];

#define N SSC_ENC_FRAME_SAMPLES

static unsigned char *read_file(const char *path, long *len) {
    FILE *f = fopen(path, "rb");
    unsigned char *buf;
    if (!f) return NULL;
    fseek(f, 0, SEEK_END);
    *len = ftell(f);
    fseek(f, 0, SEEK_SET);
    buf = (unsigned char *)malloc((size_t)*len + 1);
    if (buf && fread(buf, 1, (size_t)*len, f) != (size_t)*len) {
        free(buf);
        buf = NULL;
    }
    fclose(f);
    return buf;
}

static int cmd_enc(int argc, char **argv) {
    int rate = 44100, mono = 0, bits = 24, a, nrates = 0, k, nframes, rates_list[64];
    unsigned flags = 0;
    const char *fmt = "s16", *paths[3] = {0, 0, 0};
    int np = 0;
    long in_len, nsamp, i;
    unsigned char *raw;
    int16_t *pcm16;
    int32_t *pcm32 = NULL;
    FILE *fo, *fi;
    ssc_encoder *enc;
    for (a = 2; a < argc; a++) {
        if (!strcmp(argv[a], "-r") && a + 1 < argc) rate = atoi(argv[++a]);
        else if (!strcmp(argv[a], "-fmt") && a + 1 < argc) fmt = argv[++a];
        else if (!strcmp(argv[a], "-mono")) mono = 1;
        else if (!strcmp(argv[a], "-bits") && a + 1 < argc) bits = atoi(argv[++a]);
        else if (!strcmp(argv[a], "-flags") && a + 1 < argc) flags = (unsigned)strtoul(argv[++a], NULL, 0);
        else if (!strcmp(argv[a], "-b") && a + 1 < argc) {
            char *p = argv[++a];
            while (*p && nrates < 64) {
                rates_list[nrates++] = (int)strtol(p, &p, 0);
                if (*p == ',') p++;
            }
        } else if (np < 3) paths[np++] = argv[a];
        else return 2;
    }
    if (np != 3 || nrates == 0) {
        fprintf(stderr, "bad arguments\n");
        return 2;
    }
    raw = read_file(paths[0], &in_len);
    if (!raw) {
        fprintf(stderr, "cannot read %s\n", paths[0]);
        return 2;
    }
    if (!strcmp(fmt, "f32raw")) {
        mono = 0;
        nsamp = in_len / 4 / 2;
    } else if (!strcmp(fmt, "s32raw")) {
        nsamp = in_len / 4 / (mono ? 1 : 2);
    } else {
        nsamp = in_len / 2 / (mono ? 1 : 2);
    }
    nframes = (int)(nsamp / N);
    if (!strcmp(fmt, "s32raw")) {
        pcm32 = (int32_t *)malloc(sizeof(int32_t) * 2 * (size_t)nframes * N);
        for (i = 0; i < (long)nframes * N; i++) {
            int c;
            for (c = 0; c < 2; c++) {
                long idx = mono ? i : 2 * i + c;
                const unsigned char *b = raw + 4 * idx;
                pcm32[2 * i + c] = (int32_t)((uint32_t)b[0] | (uint32_t)b[1] << 8 | (uint32_t)b[2] << 16 |
                                             (uint32_t)b[3] << 24);
            }
        }
    }
    pcm16 = (int16_t *)malloc(sizeof(int16_t) * 2 * (size_t)nframes * N);
    for (i = 0; i < (long)nframes * N && strcmp(fmt, "f32raw") && !pcm32; i++) {
        int c;
        for (c = 0; c < 2; c++) {
            long idx = mono ? i : 2 * i + c;
            pcm16[2 * i + c] = (int16_t)(raw[2 * idx] | raw[2 * idx + 1] << 8);
        }
    }
    enc = ssc_encoder_create(rate);
    fo = fopen(paths[1], "wb");
    fi = fopen(paths[2], "w");
    if (!enc || !fo || !fi) {
        fprintf(stderr, "setup failed\n");
        return 2;
    }
    for (k = 0; k < nframes; k++) {
        unsigned char out[SSC_ENC_MAX_FRAME_BYTES];
        const int16_t *p = pcm16 + (long)k * 2 * N;
        int bitrate = rates_list[k % nrates], ret, j;
        ssc_encoder_frame_info info;
        if (pcm32) {
            ret = ssc_encode_s32(enc, pcm32 + (long)k * 2 * N, bits, bitrate, flags, out, sizeof out);
        } else if (!strcmp(fmt, "s24")) {
            int32_t s32[2 * N];
            for (j = 0; j < 2 * N; j++) s32[j] = (int32_t)p[j] * 256;
            ret = ssc_encode_s32(enc, s32, 24, bitrate, flags, out, sizeof out);
        } else if (!strcmp(fmt, "f32raw")) {
            float f[2 * N];
            memcpy(f, raw + (size_t)k * 2 * N * 4, sizeof f);
            ret = ssc_encode_float(enc, f, bitrate, flags, out, sizeof out);
        } else if (!strcmp(fmt, "f32")) {
            float f[2 * N];
            for (j = 0; j < 2 * N; j++) f[j] = (float)p[j] / 32768.f;
            ret = ssc_encode_float(enc, f, bitrate, flags, out, sizeof out);
        } else {
            ret = ssc_encode(enc, p, bitrate, flags, out, sizeof out);
        }
        if (ret < 0) {
            fprintf(stderr, "frame %d: encode error %d\n", k, ret);
            return 1;
        }
        fputc(ret & 0xFF, fo);
        fputc(ret >> 8, fo);
        fwrite(out, 1, (size_t)ret, fo);
        ssc_encoder_last_frame_info(enc, &info);
        fprintf(fi, "%d %d %d %d %d %d %d %d %d %d %d %d %d\n", info.bytes, info.bitrate, info.hq, info.seq,
                info.end_band, info.transient, info.pf_on, info.pf_period, info.pf_gain_q, info.silence,
                info.coded_bands, info.tell_bits, info.total_bits);
    }
    fclose(fo);
    fclose(fi);
    ssc_encoder_destroy(enc);
    return 0;
}

static int cmd_dec(int argc, char **argv) {
    int rate = 44100, a;
    const char *path = NULL, *pcm_path = NULL;
    long size, p = 0;
    unsigned char *d;
    ssc_decoder *dec;
    FILE *pcm_out = NULL;
    for (a = 2; a < argc; a++) {
        if (!strcmp(argv[a], "-r") && a + 1 < argc) rate = atoi(argv[++a]);
        else if (!strcmp(argv[a], "-pcm") && a + 1 < argc) pcm_path = argv[++a];
        else path = argv[a];
    }
    /* -pcm PATH: also write the decoded audio as raw f32le stereo (for fidelity measurements). */
    if (pcm_path && !(pcm_out = fopen(pcm_path, "wb"))) {
        fprintf(stderr, "cannot write %s\n", pcm_path);
        return 2;
    }
    if (!path || !(d = read_file(path, &size))) {
        fprintf(stderr, "cannot read input\n");
        return 2;
    }
    dec = ssc_decoder_create(rate);
    if (!dec) return 2;
    while (p + 2 <= size) {
        int l = d[p] | d[p + 1] << 8, ret;
        float pcm[2 * N];
        p += 2;
        if (p + l > size) break;
        ssc_debug_tell = -1;
        ret = ssc_decode_float(dec, d + p, l, pcm);
        printf("%d %d\n", ret, ssc_debug_tell);
        if (pcm_out && ret > 0) fwrite(pcm, sizeof(float), 2 * (size_t)ret, pcm_out);
        p += l;
    }
    if (pcm_out) fclose(pcm_out);
    ssc_decoder_destroy(dec);
    return 0;
}

static int cmd_tables(void) {
    int i;
    union { float f; uint32_t u; } v;
    for (i = 0; i < 108; i++) {
        v.f = sscenc_window[i];
        printf("window %d %08x\n", i, v.u);
    }
    for (i = 0; i < 25; i++) {
        v.f = sscenc_eMeans[i];
        printf("emeans %d %08x\n", i, v.u);
    }
    return 0;
}

int main(int argc, char **argv) {
    if (argc >= 2 && !strcmp(argv[1], "enc")) return cmd_enc(argc, argv);
    if (argc >= 2 && !strcmp(argv[1], "dec")) return cmd_dec(argc, argv);
    if (argc >= 2 && !strcmp(argv[1], "tables")) return cmd_tables();
    fprintf(stderr, "usage: ssc_oracle enc|dec|tables ...\n");
    return 2;
}
