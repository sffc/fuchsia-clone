// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_MEDIA_AUDIO_LIB_SIMPLE_CODEC_INCLUDE_LIB_SIMPLE_CODEC_SIMPLE_CODEC_CLIENT_H_
#define SRC_MEDIA_AUDIO_LIB_SIMPLE_CODEC_INCLUDE_LIB_SIMPLE_CODEC_SIMPLE_CODEC_CLIENT_H_

#include <fuchsia/hardware/audio/cpp/banjo.h>
#include <lib/simple-codec/simple-codec-types.h>
#include <lib/sync/completion.h>
#include <lib/zx/status.h>
#include <lib/zx/time.h>

#include <string>
#include <vector>

namespace audio {

// This class provides simple audio DAI controller drivers a way to communicate with codecs using
// the audio codec protocol. The methods in the protocol have been converted to always return a
// status in case there is not reply (after kDefaultTimeoutNsecs or the timeout specified via the
// SetTimeout() method). This class is thread hostile.
class SimpleCodecClient {
 public:
  // Convenience methods not part of the audio codec protocol.
  // Initialize the client using the DDK codec protocol object.
  zx_status_t SetProtocol(ddk::CodecProtocolClient proto_client);

  // Sync C++ methods to communicate with codecs, for descriptions see
  // //docs/concepts/drivers/driver_interfaces/audio_codec.md.
  // Methods are simplified to use standard C++ types (see simple-codec-types.h) and also:
  // - Only allow standard frame formats (DaiFrameFormatStandard, see
  //   //sdk/fidl/fuchsia.hardware.audio/dai_format.fidl).
  // - GetDaiFormats returns one DaiSupportedFormats instead of a vector (still allows supported
  //   formats with multiple frame rates, number of channels, etc. just not overly complex ones).
  // - No direct calls to WatchGainState. GetGainState queries the last gain set, the gain is to be
  //   changed only via SetGainState.
  // - No direct calls to WatchPlugState, the library only expects "hardwired" codecs.
  zx_status_t Reset();
  zx::status<Info> GetInfo();
  zx_status_t Stop();
  zx_status_t Start();
  zx::status<bool> IsBridgeable();
  zx_status_t SetBridgedMode(bool bridged);
  zx::status<DaiSupportedFormats> GetDaiFormats();
  zx_status_t SetDaiFormat(DaiFormat format);
  zx::status<GainFormat> GetGainFormat();
  zx::status<GainState> GetGainState();
  void SetGainState(GainState state);

 protected:
  ddk::CodecProtocolClient proto_client_;

 private:
  template <class T>
  struct AsyncOutData {
    sync_completion_t completion;
    zx_status_t status;
    T data;
  };

  struct AsyncOut {
    sync_completion_t completion;
    zx_status_t status;
  };

  zx::unowned_channel Connect();

  ::fuchsia::hardware::audio::CodecSyncPtr codec_;
  GainState gain_state_;
};

}  // namespace audio

#endif  // SRC_MEDIA_AUDIO_LIB_SIMPLE_CODEC_INCLUDE_LIB_SIMPLE_CODEC_SIMPLE_CODEC_CLIENT_H_
