/* Loads the built codec plugin through PipeWire's own plugin loader, the way
 * spa/plugins/bluez5/codec-loader.c does, and drives it like media-sink.c:
 * negotiation, format enumeration, then encoding real PCM into RTP packets
 * that the reference SSC decoder must decode. */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <arpa/inet.h>

#include <pipewire/pipewire.h>
#include <spa/support/plugin-loader.h>
#include <spa/param/audio/raw-utils.h>
#include <spa/debug/format.h>

#include "media-codecs.h"
#include "ssc_decoder.h"

#ifndef SLOT
#define SLOT "lc3plus"
#endif

static int failures;

#define CHECK(cond, ...) do {						\
	if (cond) { printf("ok    "); printf(__VA_ARGS__); printf("\n"); }	\
	else { failures++; printf("FAIL  "); printf(__VA_ARGS__);		\
		printf("   (%s, line %d)\n", #cond, __LINE__); }		\
} while (0)

static const uint8_t SSC_HDR[6] = { 0x75, 0x00, 0x00, 0x00, 0x03, 0x01 };

static size_t caps_of(uint8_t flags, uint8_t *out)
{
	memcpy(out, SSC_HDR, 6);
	out[6] = flags;
	return 7;
}

static int select_for(const struct media_codec *c, uint8_t sink_caps, uint32_t rate,
		const struct spa_dict *settings)
{
	uint8_t caps[A2DP_MAX_CAPS_SIZE], conf[A2DP_MAX_CAPS_SIZE];
	struct media_codec_audio_info info = { .rate = rate, .channels = 2 };
	size_t n = caps_of(sink_caps, caps);
	int res = c->select_config(c, 0, caps, n, &info, settings, conf, NULL);
	if (res < 0)
		return res;
	return res == 7 && memcmp(conf, SSC_HDR, 6) == 0 ? conf[6] : -1000;
}

static uint8_t *read_file(const char *path, size_t *size)
{
	FILE *f = fopen(path, "rb");
	uint8_t *d;
	if (!f)
		return NULL;
	fseek(f, 0, SEEK_END);
	*size = ftell(f);
	fseek(f, 0, SEEK_SET);
	d = malloc(*size);
	if (fread(d, 1, *size, f) != *size) {
		free(d);
		d = NULL;
	}
	fclose(f);
	return d;
}

struct run {
	int packets, frames, decode_errors, bad_rtp, oversize;
	int frame_bytes_first;
	uint8_t first_hdr[4];
	int frames_per_packet_max;
	uint32_t last_ts;
	long long energy;
};

/* Encode `nframes` frames of stereo s16 PCM from `pcm` (looping) like media-sink:
 * start_encode, then encode blocks until NEED_FLUSH_ALL. */
static struct run encode_run(const struct media_codec *c, void *props, uint8_t config,
		uint32_t rate, uint32_t format, size_t mtu, const int16_t *pcm, size_t pcm_frames,
		int nframes, int congest_after)
{
	struct run r = { 0 };
	uint8_t conf[7], packet[4096];
	struct spa_audio_info info = { 0 };
	void *data;
	int block, ssize = format == SPA_AUDIO_FORMAT_S32 ? 4 : 2;
	size_t pos = 0;
	uint8_t *blockbuf;
	ssc_decoder *dec = ssc_decoder_create(rate);
	int16_t out[2 * 864];

	caps_of(config, conf);
	info.media_type = SPA_MEDIA_TYPE_audio;
	info.media_subtype = SPA_MEDIA_SUBTYPE_raw;
	info.info.raw.format = format;
	info.info.raw.rate = rate;
	info.info.raw.channels = 2;

	data = c->init(c, 0, conf, sizeof(conf), &info, props, mtu);
	if (data == NULL) {
		r.packets = -1;
		ssc_decoder_destroy(dec);
		return r;
	}
	block = c->get_block_size(data);
	/* +1: hand the codec a deliberately misaligned buffer, as media-sink's tmp_buffer may be. */
	blockbuf = malloc(block + 1);

	for (uint16_t seq = 0; r.frames < nframes; seq++) {
		int hdr = c->start_encode(data, packet, sizeof(packet), seq, seq * 864);
		size_t used = hdr;
		int need_flush = NEED_FLUSH_NO, in_packet = 0;

		if (congest_after > 0 && r.packets == congest_after)
			c->reduce_bitpool(data);

		while (need_flush == NEED_FLUSH_NO) {
			size_t out_n = 0;
			uint8_t *b = blockbuf + 1;
			for (int i = 0; i < 864 * 2; i++) {
				int16_t s = pcm[(pos * 2 + i) % (pcm_frames * 2)];
				if (ssize == 4) {
					int32_t v = (int32_t)s * 65536;
					memcpy(b + 4 * i, &v, 4);
				} else {
					memcpy(b + 2 * i, &s, 2);
				}
			}
			pos += 864;
			int used_src = c->encode(data, b, block, packet + used, sizeof(packet) - used,
					&out_n, &need_flush);
			if (used_src != block) {
				r.decode_errors++;
				break;
			}
			if (r.frames == 0) {
				r.frame_bytes_first = out_n;
				memcpy(r.first_hdr, packet + used, 4);
			}
			if (ssc_decode(dec, packet + used, out_n, out) != 864)
				r.decode_errors++;
			for (int i = 0; i < 2 * 864; i++)
				r.energy += (long long)out[i] * out[i] / 1024;
			used += out_n;
			r.frames++;
			in_packet++;
		}
		if (packet[0] != 0x80 || packet[1] != 96 || ntohs(*(uint16_t *)(packet + 2)) != seq ||
		    ntohl(*(uint32_t *)(packet + 4)) != (uint32_t)r.packets ||
		    ntohl(*(uint32_t *)(packet + 8)) != 1)
			r.bad_rtp++;
		if (used > mtu)
			r.oversize++;
		if (in_packet > r.frames_per_packet_max)
			r.frames_per_packet_max = in_packet;
		r.packets++;
	}
	c->deinit(data);
	free(blockbuf);
	ssc_decoder_destroy(dec);
	return r;
}

int main(int argc, char *argv[])
{
	const char *vectors = argc > 1 ? argv[1] : "../crates/ssc-enc/tests/vectors";
	struct pw_context *context;
	struct pw_main_loop *loop;
	const struct spa_support *support;
	uint32_t n_support;
	struct spa_plugin_loader *loader;
	struct spa_handle *handle;
	void *iface;
	const struct spa_bluez5_codec_a2dp *a2dp;
	const struct media_codec *c = NULL, *uhq_codec = NULL;
	char path[512];

	pw_init(&argc, &argv);
	loop = pw_main_loop_new(NULL);
	context = pw_context_new(pw_main_loop_get_loop(loop),
			pw_properties_new(PW_KEY_CONFIG_NAME, "null", NULL), 0);
	support = pw_context_get_support(context, &n_support);
	loader = spa_support_find(support, n_support, SPA_TYPE_INTERFACE_PluginLoader);
	CHECK(loader != NULL, "PipeWire plugin loader available");

	/* Exactly what codec-loader.c asks for. */
	struct spa_dict_item items[] = { { SPA_KEY_LIBRARY_NAME, "bluez5/libspa-codec-bluez5-" SLOT } };
	struct spa_dict dict = SPA_DICT_INIT_ARRAY(items);
	handle = spa_plugin_loader_load(loader, MEDIA_CODEC_FACTORY_NAME(SLOT), &dict);
	CHECK(handle != NULL, "loader opens factory %s", MEDIA_CODEC_FACTORY_NAME(SLOT));
	if (!handle)
		return 1;
	CHECK(spa_handle_get_interface(handle, SPA_TYPE_INTERFACE_Bluez5CodecMedia, &iface) == 0,
			"handle has the Bluez5 codec interface");
	a2dp = iface;
	CHECK(a2dp->iface.version == SPA_VERSION_BLUEZ5_CODEC_MEDIA, "codec ABI version %d",
			a2dp->iface.version);
	c = a2dp->codecs[0];
	uhq_codec = c ? a2dp->codecs[1] : NULL;
	CHECK(c && uhq_codec && a2dp->codecs[2] == NULL, "two codecs: ssc and ssc_uhq");
	if (!uhq_codec)
		return 1;
	CHECK(spa_streq(uhq_codec->name, "ssc_uhq") && uhq_codec->fill_caps == NULL &&
			spa_streq(uhq_codec->endpoint_name, "ssc") && spa_streq(c->endpoint_name, "ssc") &&
			uhq_codec->id != c->id,
			"ssc_uhq (%s) shares the ssc endpoint, own id %d", uhq_codec->description, uhq_codec->id);
	CHECK(spa_streq(c->name, "ssc") && c->kind == MEDIA_CODEC_A2DP && c->codec_id == A2DP_CODEC_VENDOR &&
			c->vendor.vendor_id == 0x75 && c->vendor.codec_id == 0x0103,
			"codec %s (%s): vendor 0x%x codec 0x%04x, id %d", c->name, c->description,
			c->vendor.vendor_id, c->vendor.codec_id, c->id);
	if (c->set_log)
		c->set_log(pw_log_get());

	uint8_t caps[A2DP_MAX_CAPS_SIZE];
	int n = c->fill_caps(c, 0, NULL, caps);
	CHECK(n == 7 && memcmp(caps, SSC_HDR, 6) == 0 && caps[6] == 0x3E, "local caps 75 00 00 00 03 01 %02x", caps[6]);

	CHECK(select_for(c, 0x3E, 48000, NULL) == 0x0C, "48 kHz graph, full sink: HiFi 0x0C");
	CHECK(select_for(c, 0x3E, 96000, NULL) == 0x0C, "ssc stays HiFi at a 96 kHz graph");
	CHECK(select_for(uhq_codec, 0x3E, 48000, NULL) == 0x0A, "ssc_uhq: UHQ 0x0A");
	CHECK(select_for(uhq_codec, 0x3F, 48000, NULL) < 0, "ssc_uhq refused when the sink disables UHQ");
	CHECK(select_for(uhq_codec, 0x38, 48000, NULL) < 0, "ssc_uhq refused by a sink without UHQ");
	CHECK(select_for(c, 0x3F, 96000, NULL) == 0x0C, "sink disables UHQ: HiFi");
	CHECK(select_for(c, 0x28, 48000, NULL) == 0x28, "44.1 kHz SSC sink: 0x28");
	CHECK(select_for(c, 0x80, 48000, NULL) < 0, "legacy-only sink refused");
	struct spa_dict_item alt_items[] = { { "bluez5.a2dp.ssc.uhq-alt-config", "true" } };
	struct spa_dict alt = SPA_DICT_INIT_ARRAY(alt_items);
	CHECK(select_for(uhq_codec, 0x3E, 48000, &alt) == 0x0E, "bluez5.a2dp.ssc.uhq-alt-config: UHQ as 0x0E");
	uint8_t extra[8] = { 0x75, 0, 0, 0, 0x03, 0x01, 0x3E, 0xAA };
	uint8_t conf[A2DP_MAX_CAPS_SIZE];
	struct media_codec_audio_info ai = { 48000, 2 };
	CHECK(c->select_config(c, 0, extra, 8, &ai, NULL, conf, NULL) == 8 && conf[6] == 0x0C && conf[7] == 0,
			"sink caps with an extra octet: config as long as the caps (PipeWire requires it)");

	struct spa_audio_info vi;
	caps_of(0x0A, conf);
	CHECK(c->validate_config(c, 0, conf, 7, &vi) == 0 && vi.info.raw.rate == 96000 &&
			vi.info.raw.channels == 2 && vi.info.raw.format == SPA_AUDIO_FORMAT_S32,
			"validate UHQ: 96000 Hz, 2 ch, S32");

	uint8_t podbuf[1024];
	struct spa_pod_builder b = SPA_POD_BUILDER_INIT(podbuf, sizeof(podbuf));
	struct spa_pod *param = NULL;
	struct spa_audio_info_raw raw = { 0 };
	caps_of(0x0C, conf);
	CHECK(c->enum_config(c, 0, conf, 7, SPA_PARAM_EnumFormat, 0, &b, &param) == 1 && param,
			"enum_config HiFi returns a pod");
	if (param) {
		CHECK(spa_format_audio_raw_parse(param, &raw) >= 0 && raw.rate == 48000 && raw.channels == 2 &&
				raw.position[0] == SPA_AUDIO_CHANNEL_FL && raw.position[1] == SPA_AUDIO_CHANNEL_FR,
				"SPA parses the format: %u Hz, %u ch, format %u", raw.rate, raw.channels, raw.format);
		spa_debug_format(2, NULL, param);
	}
	CHECK(c->enum_config(c, 0, conf, 7, SPA_PARAM_EnumFormat, 1, &b, &param) == 0, "enum_config idx 1 ends");

	size_t s48, s96, sref;
	snprintf(path, sizeof(path), "%s/mix48k.s16le", vectors);
	int16_t *pcm48 = (int16_t *)read_file(path, &s48);
	snprintf(path, sizeof(path), "%s/mix96k.s16le", vectors);
	int16_t *pcm96 = (int16_t *)read_file(path, &s96);
	CHECK(pcm48 && pcm96, "test vectors in %s", vectors);
	if (!pcm48 || !pcm96)
		return 1;

	void *props = c->init_props(c, 0, NULL);
	struct run r = encode_run(c, props, 0x0C, 48000, SPA_AUDIO_FORMAT_S32, 1005, pcm48, s48 / 4, 200, 0);
	CHECK(r.packets == 200 && r.decode_errors == 0 && r.bad_rtp == 0 && r.oversize == 0 && r.energy > 0,
			"HiFi S32 mtu 1005: %d packets, %d decode errors, frame %d bytes, header %02x %02x %02x %02x",
			r.packets, r.decode_errors, r.frame_bytes_first, r.first_hdr[0], r.first_hdr[1],
			r.first_hdr[2], r.first_hdr[3]);
	CHECK(r.frame_bytes_first == 576 && r.first_hdr[3] == 0xF4, "HiFi starts at 229 kb/s (576-byte frames)");

	r = encode_run(c, props, 0x0A, 96000, SPA_AUDIO_FORMAT_S32, 1005, pcm96, s96 / 4, 200, 50);
	CHECK(r.packets > 100 && r.decode_errors == 0 && r.bad_rtp == 0 && r.oversize == 0 &&
			r.frames_per_packet_max == 2,
			"UHQ 584 then congestion -> 291 kb/s with 2 frames/packet: %d packets, max %d frames/packet",
			r.packets, r.frames_per_packet_max);

	r = encode_run(c, props, 0x0A, 96000, SPA_AUDIO_FORMAT_S32, 671, pcm96, s96 / 4, 40, 0);
	CHECK(r.packets == 20 && r.oversize == 0 && r.first_hdr[3] == 0xE6,
			"UHQ with mtu 671: 152 kb/s, 2 frames/packet, no packet over the MTU");

	r = encode_run(c, props, 0x18, 48000, SPA_AUDIO_FORMAT_S16, 1005, pcm48, s48 / 4, 50, 0);
	CHECK(r.packets == 50 && r.decode_errors == 0 && r.bad_rtp == 0, "SSC 48 kHz S16: %d packets", r.packets);

	CHECK(encode_run(c, props, 0x0C, 48000, SPA_AUDIO_FORMAT_S32, 200, pcm48, s48 / 4, 1, 0).packets == -1,
			"init refuses an MTU no rate fits");
	c->clear_props(props);

	/* Samsung's own frames (from the C encoder's conformance set) at the same rate. */
	struct spa_dict_item cap_items[] = { { "bluez5.a2dp.ssc.max-bitrate", "192" } };
	struct spa_dict cap = SPA_DICT_INIT_ARRAY(cap_items);
	props = c->init_props(c, 0, &cap);
	snprintf(path, sizeof(path), "%s/ref_frames/hifi48_192000.ssc", vectors);
	uint8_t *ref = read_file(path, &sref);
	r = encode_run(c, props, 0x0C, 48000, SPA_AUDIO_FORMAT_S32, 1005, pcm48, s48 / 4, 1, 0);
	CHECK(ref && ref[0] | ref[1] << 8, "Samsung reference frames");
	if (ref) {
		int ref_len = ref[0] | ref[1] << 8;
		CHECK(r.frame_bytes_first == ref_len && memcmp(r.first_hdr, ref + 2, 4) == 0,
				"max-bitrate 192: frame %d bytes / header %02x %02x %02x %02x == Samsung %d / %02x %02x %02x %02x",
				r.frame_bytes_first, r.first_hdr[0], r.first_hdr[1], r.first_hdr[2], r.first_hdr[3],
				ref_len, ref[2], ref[3], ref[4], ref[5]);
	}
	c->clear_props(props);

	spa_plugin_loader_unload(loader, handle);
	pw_context_destroy(context);
	pw_main_loop_destroy(loop);
	pw_deinit();
	printf(failures ? "\n%d FAILED\n" : "\nall checks passed\n", failures);
	return failures ? 1 : 0;
}
