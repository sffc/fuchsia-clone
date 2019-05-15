// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//+build pprof

package netstack

import (
	"syslog"

	"net/http"
	_ "net/http/pprof"
)

func pprofListen() {
	syslog.Infof("starting http server on port 6060")
	syslog.Infof("%v", http.ListenAndServe(":6060", nil))
}
