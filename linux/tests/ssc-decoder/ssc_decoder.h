/* SSC (Samsung Scalable Codec) decoder: public API.
 *
 * One SSC frame = 4-byte header + range-coded payload, decoding to 864 stereo samples.
 * The output lags the encoder input by 108 samples (MDCT overlap).
 *
 * Memory: all tables are const (they stay in flash on MCUs). The working state (~44 KB) is only
 * allocated by ssc_decoder_create*, as 6 blocks of at most ~15 KB, and freed by ssc_decoder_destroy.
 * The library has no static RAM buffers.
 */
#ifndef SSC_DECODER_H
#define SSC_DECODER_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#define SSC_FRAME_SAMPLES 864

typedef struct ssc_decoder ssc_decoder;

typedef struct {
    int bitrate;   /* bits per second, from the header's rate code */
    int hq;        /* "high quality" rate class (E-family, 256k, 328k) */
    int seq;       /* frame sequence counter (4 or 8 bits) */
    int end_band;  /* number of coded bands (18..22) */
} ssc_frame_info;

/* Memory hooks. */
typedef void *(*ssc_alloc_fn)(void *ctx, unsigned size);
typedef void (*ssc_free_fn)(void *ctx, void *ptr);

/* sample_rate: 44100 or 48000 (same codec mode) or 96000. Returns NULL on bad rate / no memory. */
ssc_decoder *ssc_decoder_create(int sample_rate);
ssc_decoder *ssc_decoder_create_with_allocator(int sample_rate, ssc_alloc_fn alloc, ssc_free_fn release,
                                               void *ctx);
void ssc_decoder_destroy(ssc_decoder *dec);
/* Total bytes the decoder allocates. */
int ssc_decoder_size(void);
/* Clear all history (e.g. after a stream gap). */
void ssc_decoder_reset(ssc_decoder *dec);

/* Parse only the 4-byte header. Returns 0 if valid. */
int ssc_parse_frame_info(const uint8_t *frame, int len, ssc_frame_info *info);

/* Decode one frame into 864 interleaved stereo int16 samples (pcm must hold 1728 values).
 * Returns 864 on success, negative on error (pcm is then filled with silence). */
int ssc_decode(ssc_decoder *dec, const uint8_t *frame, int len, int16_t *pcm);

/* Same, float output in [-1, 1). */
int ssc_decode_float(ssc_decoder *dec, const uint8_t *frame, int len, float *pcm);

/* Same, int32 output with 16-bit full scale at 1<<31 (keeps the extra precision of 24-bit streams). */
int ssc_decode_s32(ssc_decoder *dec, const uint8_t *frame, int len, int32_t *pcm);

/* Total size in bytes (header included) of the frame starting at `frame`, derived from its header
 * (the wire format has no length field). sample_rate selects the codec mode: 44100/48000 or 96000.
 * Returns 0 if the header is invalid. Needed when one packet carries several frames (96 kHz). */
int ssc_frame_bytes(const uint8_t *frame, int avail, int sample_rate);

#ifdef __cplusplus
}
#endif
#endif
