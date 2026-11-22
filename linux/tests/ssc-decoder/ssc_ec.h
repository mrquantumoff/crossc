/* SSC range decoder (opus 1.1 entdec with EC_CODE_BITS = 24). Internal header. */
#ifndef SSC_EC_H
#define SSC_EC_H

#include <stdint.h>

typedef uint32_t opus_uint32;
typedef int32_t opus_int32;
typedef int16_t opus_int16;
typedef int16_t opus_val16;
typedef int32_t opus_val32;
typedef opus_uint32 ec_window;

/* Private names: these would otherwise clash with libopus (24-bit vs 32-bit range coder) when both are linked. */
#define ec_dec_init ssc_ec_dec_init
#define ec_tell ssc_ec_tell
#define ec_tell_frac ssc_ec_tell_frac
#define ec_decode ssc_ec_decode
#define ec_decode_bin ssc_ec_decode_bin
#define ec_dec_update ssc_ec_dec_update
#define ec_dec_bit_logp ssc_ec_dec_bit_logp
#define ec_dec_icdf ssc_ec_dec_icdf
#define ec_dec_bits ssc_ec_dec_bits
#define ec_dec_uint ssc_ec_dec_uint
#define ec_laplace_decode ssc_ec_laplace_decode
#define ec_dec ssc_ec_dec

/* Same layout as opus 1.1 ec_ctx. */
typedef struct ec_dec {
    unsigned char *buf;
    opus_uint32 storage;
    opus_uint32 end_offs;
    ec_window end_window;
    int nend_bits;
    int nbits_total;
    opus_uint32 offs;
    opus_uint32 rng;
    opus_uint32 val;
    opus_uint32 ext;
    int rem;
    int error;
} ec_dec;

void ec_dec_init(ec_dec *d, unsigned char *buf, opus_uint32 storage);
int ec_tell(ec_dec *d);
opus_uint32 ec_tell_frac(ec_dec *d);
unsigned ec_decode(ec_dec *d, unsigned ft);
unsigned ec_decode_bin(ec_dec *d, unsigned bits);
void ec_dec_update(ec_dec *d, unsigned fl, unsigned fh, unsigned ft);
int ec_dec_bit_logp(ec_dec *d, unsigned logp);
int ec_dec_icdf(ec_dec *d, const unsigned char *icdf, unsigned ftb);
opus_uint32 ec_dec_bits(ec_dec *d, unsigned bits);
opus_uint32 ec_dec_uint(ec_dec *d, opus_uint32 ft);
int ec_laplace_decode(ec_dec *dec, unsigned fs, int decay);

/* Energy dequantisation (ssc_energy.c). Energies are int16 Q10 log2 units, layout [c*22 + band]. */
void ssc_unquant_coarse_energy(int nbEBands, int start, int end, opus_val16 *oldEBands,
                               int intra, ec_dec *dec, int C, opus_int32 budget);
void ssc_unquant_fine_energy(int nbEBands, int start, int end, opus_val16 *oldEBands,
                             const int *fine_quant, ec_dec *dec, int C);
void ssc_unquant_energy_finalise(int nbEBands, int start, int end, opus_val16 *oldEBands,
                                 const int *fine_quant, const int *fine_priority,
                                 int bits_left, ec_dec *dec, int C);

#endif
