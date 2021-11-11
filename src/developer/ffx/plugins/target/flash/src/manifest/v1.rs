// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use {
    anyhow::Result,
    async_trait::async_trait,
    errors::ffx_bail,
    ffx_fastboot_common::{
        file::FileResolver, flash_and_reboot, is_locked, Flash, Partition as PartitionTrait,
        Product as ProductTrait, MISSING_PRODUCT, UNLOCK_ERR,
    },
    ffx_flash_args::{FlashCommand, OemFile},
    fidl_fuchsia_developer_bridge::FastbootProxy,
    serde::{Deserialize, Serialize},
    std::io::Write,
};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct Product {
    pub(crate) name: String,
    pub(crate) bootloader_partitions: Vec<Partition>,
    pub(crate) partitions: Vec<Partition>,
    pub(crate) oem_files: Vec<OemFile>,
    #[serde(default)]
    pub(crate) requires_unlock: bool,
}

impl ProductTrait<Partition> for Product {
    fn bootloader_partitions(&self) -> &Vec<Partition> {
        &self.bootloader_partitions
    }

    fn partitions(&self) -> &Vec<Partition> {
        &self.partitions
    }

    fn oem_files(&self) -> &Vec<OemFile> {
        &self.oem_files
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct Partition(
    String,
    String,
    #[serde(default)] Option<String>,
    #[serde(default)] Option<String>,
);

impl Partition {
    pub(crate) fn new(
        name: String,
        file: String,
        variable: Option<String>,
        variable_value: Option<String>,
    ) -> Self {
        Self(name, file, variable, variable_value)
    }
}

impl PartitionTrait for Partition {
    fn name(&self) -> &str {
        self.0.as_str()
    }

    fn file(&self) -> &str {
        self.1.as_str()
    }

    fn variable(&self) -> Option<&str> {
        self.2.as_ref().map(|s| s.as_str())
    }

    fn variable_value(&self) -> Option<&str> {
        self.3.as_ref().map(|s| s.as_str())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct FlashManifest(pub(crate) Vec<Product>);

#[async_trait(?Send)]
impl Flash for FlashManifest {
    async fn flash<W, F>(
        &self,
        writer: &mut W,
        file_resolver: &mut F,
        fastboot_proxy: FastbootProxy,
        cmd: FlashCommand,
    ) -> Result<()>
    where
        W: Write,
        F: FileResolver + Sync,
    {
        let product = match self.0.iter().find(|product| product.name == cmd.product) {
            Some(res) => res,
            None => ffx_bail!("{} {}", MISSING_PRODUCT, cmd.product),
        };
        if product.requires_unlock && is_locked(&fastboot_proxy).await? {
            ffx_bail!("{}", UNLOCK_ERR);
        }
        flash_and_reboot(writer, file_resolver, product, &fastboot_proxy, cmd).await
    }
}

////////////////////////////////////////////////////////////////////////////////
// tests

#[cfg(test)]
mod test {
    use super::*;
    use crate::test::{setup, TestResolver};
    use regex::Regex;
    use serde_json::from_str;
    use std::path::PathBuf;
    use tempfile::NamedTempFile;

    const SIMPLE_MANIFEST: &'static str = r#"[
            {
                "name": "fuchsia",
                "bootloader_partitions": [],
                "partitions": [
                    ["test1", "path1"],
                    ["test2", "path2"],
                    ["test3", "path3"],
                    ["test4", "path4"],
                    ["test5", "path5"]
                ],
                "oem_files": []
            }
    ]"#;

    const MANIFEST: &'static str = r#"[
        {
            "name": "zedboot",
            "bootloader_partitions": [
                ["test1", "path1"],
                ["test2", "path2"]
            ],
            "partitions": [
                ["test1", "path1"],
                ["test2", "path2"],
                ["test3", "path3"],
                ["test4", "path4"],
                ["test5", "path5"]
            ],
            "oem_files": [
                ["test1", "path1"],
                ["test2", "path2"]
            ]
        },
        {
            "name": "fuchsia",
            "bootloader_partitions": [],
            "partitions": [
                ["test10", "path10"],
                ["test20", "path20"],
                ["test30", "path30"]
            ],
            "oem_files": []
        }
    ]"#;

    const CONDITIONAL_MANIFEST: &'static str = r#"[
        {
            "name": "zedboot",
            "bootloader_partitions": [
                ["btest1", "bpath1", "var1", "value1"],
                ["btest2", "bpath2", "var2", "value2"],
                ["btest3", "bpath3", "var3", "value3"]
            ],
            "partitions": [],
            "oem_files": []
        }
    ]"#;

    const LOCKED_MANIFEST: &'static str = r#"[
        {
            "name": "zedboot",
            "bootloader_partitions": [
                ["btest1", "bpath1", "var1", "value1"]
            ],
            "partitions": [],
            "oem_files": [],
            "requires_unlock": true
        }
    ]"#;

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_deserializing_should_work() -> Result<()> {
        let v: FlashManifest = from_str(MANIFEST)?;
        let zedboot: &Product = &v.0[0];
        assert_eq!("zedboot", zedboot.name);
        assert_eq!(2, zedboot.bootloader_partitions.len());
        let bootloader_expected = [["test1", "path1"], ["test2", "path2"]];
        for x in 0..bootloader_expected.len() {
            assert_eq!(zedboot.bootloader_partitions[x].name(), bootloader_expected[x][0]);
            assert_eq!(zedboot.bootloader_partitions[x].file(), bootloader_expected[x][1]);
        }
        assert_eq!(5, zedboot.partitions.len());
        let expected = [
            ["test1", "path1"],
            ["test2", "path2"],
            ["test3", "path3"],
            ["test4", "path4"],
            ["test5", "path5"],
        ];
        for x in 0..expected.len() {
            assert_eq!(zedboot.partitions[x].name(), expected[x][0]);
            assert_eq!(zedboot.partitions[x].file(), expected[x][1]);
        }
        assert_eq!(2, zedboot.oem_files.len());
        let oem_files_expected = [["test1", "path1"], ["test2", "path2"]];
        for x in 0..oem_files_expected.len() {
            assert_eq!(zedboot.oem_files[x].command(), oem_files_expected[x][0]);
            assert_eq!(zedboot.oem_files[x].file(), oem_files_expected[x][1]);
        }
        let product: &Product = &v.0[1];
        assert_eq!("fuchsia", product.name);
        assert_eq!(0, product.bootloader_partitions.len());
        assert_eq!(3, product.partitions.len());
        let expected2 = [["test10", "path10"], ["test20", "path20"], ["test30", "path30"]];
        for x in 0..expected2.len() {
            assert_eq!(product.partitions[x].name(), expected2[x][0]);
            assert_eq!(product.partitions[x].file(), expected2[x][1]);
        }
        assert_eq!(0, product.oem_files.len());
        Ok(())
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_should_fail_if_product_missing() -> Result<()> {
        let tmp_file = NamedTempFile::new().expect("tmp access failed");
        let tmp_file_name = tmp_file.path().to_string_lossy().to_string();
        let v: FlashManifest = from_str(MANIFEST)?;
        let (_, proxy) = setup();
        let mut writer = Vec::<u8>::new();
        assert!(v
            .flash(
                &mut writer,
                &mut TestResolver::new(),
                proxy,
                FlashCommand {
                    manifest: Some(PathBuf::from(tmp_file_name)),
                    product: "Unknown".to_string(),
                    ..Default::default()
                }
            )
            .await
            .is_err());
        Ok(())
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_should_succeed_if_product_found() -> Result<()> {
        let v: FlashManifest = from_str(MANIFEST)?;
        let tmp_file = NamedTempFile::new().expect("tmp access failed");
        let tmp_file_name = tmp_file.path().to_string_lossy().to_string();
        let (_, proxy) = setup();
        let mut writer = Vec::<u8>::new();
        v.flash(
            &mut writer,
            &mut TestResolver::new(),
            proxy,
            FlashCommand {
                manifest: Some(PathBuf::from(tmp_file_name)),
                product: "fuchsia".to_string(),
                ..Default::default()
            },
        )
        .await?;
        let output = String::from_utf8(writer).expect("utf-8 string");
        for partition in &v.0[1].partitions {
            let name_listing = Regex::new(&partition.name()).expect("test regex");
            let path_listing = Regex::new(&partition.file()).expect("test regex");
            assert_eq!(name_listing.find_iter(&output).count(), 1);
            assert_eq!(path_listing.find_iter(&output).count(), 1);
        }
        Ok(())
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_oem_file_should_be_staged_from_command() -> Result<()> {
        let v: FlashManifest = from_str(SIMPLE_MANIFEST)?;
        let (state, proxy) = setup();
        let test_oem_cmd = "test-oem-cmd";
        let tmp_file = NamedTempFile::new().expect("tmp access failed");
        let tmp_file_name = tmp_file.path().to_string_lossy().to_string();
        let test_staged_file = format!("{},{}", test_oem_cmd, tmp_file_name).parse::<OemFile>()?;
        let manifest_file = NamedTempFile::new().expect("tmp access failed");
        let manifest_file_name = manifest_file.path().to_string_lossy().to_string();
        let mut writer = Vec::<u8>::new();
        v.flash(
            &mut writer,
            &mut TestResolver::new(),
            proxy,
            FlashCommand {
                manifest: Some(PathBuf::from(manifest_file_name)),
                product: "fuchsia".to_string(),
                oem_stage: vec![test_staged_file],
                ..Default::default()
            },
        )
        .await?;
        let state = state.lock().unwrap();
        assert_eq!(1, state.staged_files.len());
        assert_eq!(1, state.oem_commands.len());
        assert_eq!(test_oem_cmd, state.oem_commands[0]);
        Ok(())
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_should_upload_conditional_partitions_that_match() -> Result<()> {
        let v: FlashManifest = from_str(CONDITIONAL_MANIFEST)?;
        let tmp_file = NamedTempFile::new().expect("tmp access failed");
        let tmp_file_name = tmp_file.path().to_string_lossy().to_string();
        let (state, proxy) = setup();
        {
            let mut state = state.lock().unwrap();
            state.variables.push("not_value3".to_string());
            state.variables.push("value2".to_string());
            state.variables.push("not_value1".to_string());
        }
        let mut writer = Vec::<u8>::new();
        v.flash(
            &mut writer,
            &mut TestResolver::new(),
            proxy,
            FlashCommand {
                manifest: Some(PathBuf::from(tmp_file_name)),
                product: "zedboot".to_string(),
                ..Default::default()
            },
        )
        .await?;
        let output = String::from_utf8(writer).expect("utf-8 string");
        for (i, partition) in v.0[0].bootloader_partitions.iter().enumerate() {
            let name_listing = Regex::new(&partition.name()).expect("test regex");
            let path_listing = Regex::new(&partition.file()).expect("test regex");
            let expected = if i == 1 { 1 } else { 0 };
            assert_eq!(name_listing.find_iter(&output).count(), expected);
            assert_eq!(path_listing.find_iter(&output).count(), expected);
        }
        Ok(())
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_should_succeed_and_not_reboot_bootloader() -> Result<()> {
        let v: FlashManifest = from_str(MANIFEST)?;
        let tmp_file = NamedTempFile::new().expect("tmp access failed");
        let tmp_file_name = tmp_file.path().to_string_lossy().to_string();
        let (state, proxy) = setup();
        let mut writer = Vec::<u8>::new();
        v.flash(
            &mut writer,
            &mut TestResolver::new(),
            proxy,
            FlashCommand {
                manifest: Some(PathBuf::from(tmp_file_name)),
                product: "fuchsia".to_string(),
                no_bootloader_reboot: true,
                ..Default::default()
            },
        )
        .await?;
        let state = state.lock().unwrap();
        assert_eq!(0, state.bootloader_reboots);
        Ok(())
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_should_not_flash_if_target_is_locked_and_product_requires_unlock() -> Result<()> {
        let v: FlashManifest = from_str(LOCKED_MANIFEST)?;
        let tmp_file = NamedTempFile::new().expect("tmp access failed");
        let tmp_file_name = tmp_file.path().to_string_lossy().to_string();
        let (state, proxy) = setup();
        {
            let mut state = state.lock().unwrap();
            state.variables.push("vx-locked".to_string());
            state.variables.push("yes".to_string());
        }
        let mut writer = Vec::<u8>::new();
        let res = v
            .flash(
                &mut writer,
                &mut TestResolver::new(),
                proxy,
                FlashCommand {
                    manifest: Some(PathBuf::from(tmp_file_name)),
                    product: "zedboot".to_string(),
                    ..Default::default()
                },
            )
            .await;
        assert_eq!(true, res.is_err());
        Ok(())
    }
}
