// The one call wirephoto needs from jpegli: 8-bit RGB in, a baseline 4:2:0 JPEG out, with the settings cjpegli
// uses for `-q Q --chroma_subsampling=420 -p 0` (adaptive quantisation, optimised Huffman tables).
#include <setjmp.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>

#include "lib/jpegli/common.h"
#include "lib/jpegli/encode.h"

namespace {

struct ErrorMgr {
  jpeg_error_mgr pub;
  jmp_buf jump;
};

void ErrorExit(j_common_ptr cinfo) {
  longjmp(reinterpret_cast<ErrorMgr*>(cinfo->err)->jump, 1);
}

void NoOutput(j_common_ptr) {}

}  // namespace

extern "C" {

// Returns 0 and a malloc'd buffer in *out (free with wirephoto_jpegli_free), or nonzero with the message in msg.
int wirephoto_jpegli_encode_rgb(const uint8_t* px, uint32_t width, uint32_t height, int quality,
                                uint8_t** out, size_t* out_len, char* msg /* JMSG_LENGTH_MAX */) {
  jpeg_compress_struct cinfo;
  ErrorMgr err;
  unsigned char* buf = nullptr;
  unsigned long len = 0;  // NOLINT
  *out = nullptr;
  *out_len = 0;
  cinfo.err = jpegli_std_error(&err.pub);
  err.pub.error_exit = ErrorExit;
  err.pub.output_message = NoOutput;
  if (setjmp(err.jump)) {
    (*cinfo.err->format_message)(reinterpret_cast<j_common_ptr>(&cinfo), msg);
    jpegli_destroy_compress(&cinfo);
    free(buf);
    return 1;
  }
  jpegli_create_compress(&cinfo);
  jpegli_mem_dest(&cinfo, &buf, &len);
  cinfo.image_width = width;
  cinfo.image_height = height;
  cinfo.input_components = 3;
  cinfo.in_color_space = JCS_RGB;
  jpegli_set_defaults(&cinfo);
  cinfo.comp_info[0].h_samp_factor = 2;
  cinfo.comp_info[0].v_samp_factor = 2;
  for (int i = 1; i < cinfo.num_components; i++) {
    cinfo.comp_info[i].h_samp_factor = 1;
    cinfo.comp_info[i].v_samp_factor = 1;
  }
  jpegli_enable_adaptive_quantization(&cinfo, TRUE);
  jpegli_set_distance(&cinfo, jpegli_quality_to_distance(quality), TRUE);
  jpegli_set_progressive_level(&cinfo, 0);
  cinfo.optimize_coding = TRUE;
  jpegli_set_input_format(&cinfo, JPEGLI_TYPE_UINT8, JPEGLI_NATIVE_ENDIAN);
  cinfo.write_JFIF_header = FALSE;
  cinfo.write_Adobe_marker = FALSE;
  jpegli_start_compress(&cinfo, TRUE);
  const size_t stride = static_cast<size_t>(width) * 3;
  while (cinfo.next_scanline < cinfo.image_height) {
    JSAMPROW row = const_cast<JSAMPROW>(px + cinfo.next_scanline * stride);
    jpegli_write_scanlines(&cinfo, &row, 1);
  }
  jpegli_finish_compress(&cinfo);
  jpegli_destroy_compress(&cinfo);
  *out = buf;
  *out_len = len;
  return 0;
}

void wirephoto_jpegli_free(uint8_t* p) { free(p); }

}  // extern "C"
