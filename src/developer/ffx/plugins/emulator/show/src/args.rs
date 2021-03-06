// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use argh::FromArgs;
use ffx_core::ffx_command;

#[ffx_command()]
#[derive(FromArgs, Default, Debug, PartialEq)]
#[argh(subcommand, name = "show")]
/// Show Fuchsia emulator details.
pub struct ShowCommand {
    /// name of the emulator instance to show details for.
    /// See a list of available instances by running `ffx emu list`.
    /// Defaults to "fuchsia-emulator".
    #[argh(positional, default = "\"fuchsia-emulator\".to_string()")]
    pub name: String,
}
