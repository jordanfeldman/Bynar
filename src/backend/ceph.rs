use std::collections::BTreeMap;
use std::fmt;
use std::fs::{
    create_dir, read_dir, read_to_string, remove_dir_all, symlink_metadata, File, OpenOptions,
};
use std::io::Write;
use std::os::unix::{fs::symlink, io::AsRawFd};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::str::FromStr;

use crate::backend::Backend;

use blkid::BlkId;
use ceph::ceph::{connect_to_ceph, Rados};
use ceph::cmd::*;
use ceph::CephVersion;
use ceph_safe_disk::diag::{DiagMap, Format, Status};
use dirs::home_dir;
use fstab::FsTab;
use helpers::{error::*, host_information::Host};
use init_daemon::{detect_daemon, Daemon};
use log::{debug, error, info, trace};
use lvm::*;
use nix::{
    convert_ioctl_res, ioc, ioctl_none, request_code_none,
    unistd::chown,
    unistd::{Gid, Uid},
};
use pwd::Passwd;
use serde_derive::*;
use tempdir::TempDir;

/// Ceph cluster
pub struct CephBackend {
    cluster_handle: Rados,
    config: CephConfig,
    version: CephVersion,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
struct JournalDevice {
    device: PathBuf,
    partition_id: Option<u32>,
    partition_uuid: Option<uuid::Uuid>,
    num_partitions: Option<usize>,
}

impl JournalDevice {
    /// Discover the number of partitions on the device and
    /// update the num_partitions field
    fn update_num_partitions(&mut self) -> BynarResult<()> {
        let num_parts = gpt::GptConfig::new()
            .writable(false)
            .initialized(true)
            .open(&self.device)?
            .partitions()
            .len();
        self.num_partitions = Some(num_parts);

        Ok(())
    }
}

impl fmt::Display for JournalDevice {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self.partition_id {
            Some(id) => write!(f, "{}{}", self.device.display(), id),
            None => write!(f, "{}", self.device.display()),
        }
    }
}

#[test]
fn test_journal_sorting() {
    let a = JournalDevice {
        device: PathBuf::from("/dev/sda"),
        partition_id: None,
        partition_uuid: None,
        num_partitions: Some(2),
    };
    let b = JournalDevice {
        device: PathBuf::from("/dev/sdb"),
        partition_id: None,
        partition_uuid: None,
        num_partitions: Some(1),
    };
    let mut journal_devices = vec![a.clone(), b.clone()];
    journal_devices.sort_by_key(|j| j.num_partitions);
    println!("journal_devices: {:?}", journal_devices);
    // Journal devicess should be sorted from least to greatest number
    // of partitions
    assert_eq!(journal_devices, vec![b, a]);
}

#[derive(Deserialize, Debug)]
struct CephConfig {
    /// The location of the ceph.conf file
    config_file: String,
    /// The cephx user to connect to the Ceph service with
    user_id: String,
    /// The /dev/xxx devices to use for journal partitions.
    /// Bynar will create new partitions on these devices as needed
    /// if no journal_partition_id is given
    journal_devices: Option<Vec<JournalDevice>>,
}

fn choose_ceph_config(config_dir: Option<&Path>) -> BynarResult<PathBuf> {
    match config_dir {
        Some(config) => {
            let json_path = config.join("ceph.json");
            if !json_path.exists() {
                let err_msg = format!("{} does not exist.  Please create", json_path.display());
                error!("{}", err_msg);
                return Err(BynarError::new(err_msg));
            }
            debug!("Loading ceph config from: {}", json_path.display(),);
            Ok(json_path)
        }
        None => {
            let home = home_dir().expect("HOME env variable not defined");
            let json_path = home.join(".config").join("ceph.json");
            if !json_path.exists() {
                let err_msg = format!("{} does not exist.  Please create", json_path.display());
                error!("{}", err_msg);
                return Err(BynarError::new(err_msg));
            }
            info!("Reading ceph config file: {}", json_path.display(),);
            Ok(json_path)
        }
    }
}

impl CephBackend {
    pub fn new(config_dir: Option<&Path>) -> BynarResult<CephBackend> {
        let ceph_config = choose_ceph_config(config_dir)?;
        if !ceph_config.exists() {
            error!("ceph config {} does not exist", ceph_config.display());
        }
        let s = read_to_string(ceph_config)?;
        let deserialized: CephConfig = serde_json::from_str(&s)?;

        info!("Connecting to Ceph");
        let cluster_handle = connect_to_ceph(&deserialized.user_id, &deserialized.config_file)?;
        info!("Connected to Ceph");
        let version_str = version(&cluster_handle)?;
        let version: CephVersion = version_str.parse()?;
        Ok(CephBackend {
            cluster_handle,
            config: deserialized,
            version,
        })
    }

    fn add_bluestore_osd(
        &self,
        dev_path: &Path,
        id: Option<u64>,
        simulate: bool,
    ) -> BynarResult<()> {
        /*
        //TODO  What is the deal with this tmpfs??
        mount, "-t", "tmpfs", "tmpfs", "/var/lib/ceph/osd/ceph-2"
            */
        // Create the journal device if requested
        let journal = self.select_journal()?;

        // Create a new osd id
        let new_osd_id = osd_create(&self.cluster_handle, id, simulate)?;
        debug!("New osd id created: {:?}", new_osd_id);
        let osd_fsid = uuid::Uuid::new_v4();
        let (lv_dev_name, vg_size) =
            self.create_lvm(&osd_fsid, new_osd_id, &dev_path, journal.as_ref())?;

        // Mount the drive
        let mount_point = Path::new("/var/lib/ceph/osd").join(&format!("ceph-{}", new_osd_id));
        if !mount_point.exists() {
            debug!(
                "Mount point {} doesn't exist.  Creating.",
                mount_point.display()
            );
            create_dir(&mount_point)?;
        }
        // Write out osd fsid to a file
        let fsid_path = mount_point.join("fsid");
        debug!("opening {} for writing", fsid_path.display());
        let mut activate_file = File::create(&fsid_path)?;
        activate_file
            .write_all(&format!("{}\n", osd_fsid.to_hyphenated().to_string()).as_bytes())?;

        // LVM's logical volume name is a symlink to the true device
        // This finds that device and then we chown it so ceph can use it
        let backer_device = self.resolve_lvm_device(&lv_dev_name)?;
        debug!("Resolved lvm device to {}", backer_device.display());
        debug!(
            "Symlinking {} to {}",
            lv_dev_name.display(),
            mount_point.join("block").display()
        );
        symlink(&lv_dev_name, mount_point.join("block"))?;
        // Optionally symlink the journal if using one
        if let Some(journal) = &journal {
            symlink(
                &Path::new(&format!("{}", journal)),
                mount_point.join("block.wal"),
            )?;
            let ceph_user = Passwd::from_name("ceph")?
                .ok_or_else(|| BynarError::from("ceph user id not found"))?;
            self.change_permissions(&[&Path::new(&format!("{}", journal))], &ceph_user)?;
        }

        // Write activate monmap out
        debug!("Getting latest monmap from ceph");
        let activate_monmap = mon_getmap(&self.cluster_handle, None)?;
        let activate_path = mount_point.join("activate.monmap");
        debug!("opening {} for writing", activate_path.display());
        let mut activate_file = File::create(&activate_path)?;
        activate_file.write_all(&activate_monmap)?;

        debug!("Looking up ceph user id");
        let ceph_user =
            Passwd::from_name("ceph")?.ok_or_else(|| BynarError::from("ceph user id not found"))?;
        self.change_permissions(
            &[&backer_device, &activate_path, &mount_point, &fsid_path],
            &ceph_user,
        )?;
        debug!("Creating ceph authorization entry");
        osd_auth_add(&self.cluster_handle, new_osd_id, simulate)?;
        let auth_key = auth_get_key(&self.cluster_handle, "osd", &new_osd_id.to_string())?;
        debug!("Saving ceph keyring");
        save_keyring(new_osd_id, &auth_key, Some(0), Some(0), simulate)?;

        // Format the osd with the osd filesystem
        ceph_mkfs(
            new_osd_id,
            journal.as_ref(),
            true,
            Some(&activate_path),
            Some(&mount_point),
            Some(&osd_fsid),
            Some("ceph"),
            Some("ceph"),
            simulate,
        )?;
        ceph_bluestore_tool(&lv_dev_name, &mount_point, simulate)?;

        let host_info = Host::new()?;
        let gb_capacity = vg_size / 1_073_741_824;
        let osd_weight = gb_capacity as f64 * 0.001_f64;
        debug!(
            "Adding OSD {} to crushmap under host {} with weight: {}",
            new_osd_id, host_info.hostname, osd_weight
        );
        osd_crush_add(
            &self.cluster_handle,
            new_osd_id,
            osd_weight,
            &host_info.hostname,
            simulate,
        )?;
        systemctl_enable(new_osd_id, &osd_fsid, simulate)?;
        setup_osd_init(new_osd_id, simulate)?;
        Ok(())
    }

    /// Add a new /dev/ path as an osd.
    // Add osds with xfs
    // Jewel or earlier
    fn add_filestore_osd(
        &self,
        dev_path: &Path,
        id: Option<u64>,
        simulate: bool,
    ) -> BynarResult<()> {
        //Format the drive
        let xfs_options = block_utils::Filesystem::Xfs {
            stripe_size: None,
            stripe_width: None,
            block_size: None,
            agcount: Some(32),
            inode_size: Some(2048),
            force: true,
        };
        debug!(
            "Formatting {:?} with XFS options: {:?}",
            dev_path, xfs_options
        );
        if !simulate {
            block_utils::format_block_device(dev_path, &xfs_options)?;
            let _ = settle_udev();
        }

        // Probe the drive
        debug!("udev Probing device {:?}", dev_path);
        let info = block_utils::get_device_info(dev_path)?;
        debug!("udev info {:?}", info);
        if info.id.is_none() {
            return Err(BynarError::new(format!(
                "Formatted device {:?} doesn't have a filesystem UUID.  Please investigate",
                dev_path
            )));
        }

        // Create a new osd id
        let new_osd_id = osd_create(&self.cluster_handle, id, simulate)?;
        debug!("New osd id created: {:?}", new_osd_id);

        // Mount the drive
        let mount_point = Path::new("/var/lib/ceph/osd/").join(format!("ceph-{}", new_osd_id));
        if !simulate {
            if !mount_point.exists() {
                debug!(
                    "Mount point {} doesn't exist.  Creating.",
                    mount_point.display()
                );
                create_dir(&mount_point)?;
            }
            block_utils::mount_device(&info, &mount_point)?;
        }

        let journal = self.select_journal()?;

        // Format the osd with the osd filesystem
        ceph_mkfs(
            new_osd_id,
            journal.as_ref(),
            false,
            None,
            None,
            None,
            None,
            None,
            simulate,
        )?;
        debug!("Creating ceph authorization entry");
        osd_auth_add(&self.cluster_handle, new_osd_id, simulate)?;
        let auth_key = auth_get_key(&self.cluster_handle, "osd", &new_osd_id.to_string())?;
        debug!("Saving ceph keyring");
        save_keyring(new_osd_id, &auth_key, None, None, simulate)?;
        let host_info = Host::new()?;
        let gb_capacity = info.capacity / 1_073_741_824;
        let osd_weight = gb_capacity as f64 * 0.001_f64;
        debug!(
            "Adding OSD {} to crushmap under host {} with weight: {}",
            new_osd_id, host_info.hostname, osd_weight
        );
        osd_crush_add(
            &self.cluster_handle,
            new_osd_id,
            osd_weight,
            &host_info.hostname,
            simulate,
        )?;
        add_osd_to_fstab(&info, new_osd_id, simulate)?;
        // This step depends on whether it's systemctl, upstart, etc
        setup_osd_init(new_osd_id, simulate)?;
        Ok(())
    }

    // Change permissions of many files at once
    fn change_permissions(&self, paths: &[&Path], perms: &Passwd) -> BynarResult<()> {
        for p in paths {
            debug!("chown {} with {}:{}", p.display(), perms.uid, perms.gid);
            chown(
                *p,
                Some(Uid::from_raw(perms.uid)),
                Some(Gid::from_raw(perms.gid)),
            )?;
        }
        Ok(())
    }

    // Create the LVM device and return the path and size of it
    fn create_lvm(
        &self,
        osd_fsid: &uuid::Uuid,
        new_osd_id: u64,
        dev_path: &Path,
        journal_device: Option<&JournalDevice>,
    ) -> BynarResult<(PathBuf, u64)> {
        debug!("udev Probing device {:?}", dev_path);
        let info = block_utils::get_device_info(dev_path)?;
        debug!("udev info {:?}", info);
        let vg_name = format!("ceph-{}", uuid::Uuid::new_v4());
        let lv_name = format!("osd-block-{}", osd_fsid);
        let lv_dev_name = Path::new("/dev").join(&vg_name).join(&lv_name);
        debug!("initializing LVM");
        let lvm = Lvm::new(None)?;
        lvm.scan()?;
        debug!("Creating volume group: {}", vg_name);
        let vg = lvm.vg_create(&vg_name)?;
        debug!("Adding {} to volume group", dev_path.display());
        vg.extend(dev_path)?;
        vg.write()?;
        debug!(
            "Creating logical volume: {} of size: {} with {} extents free.  Extent size: {}",
            lv_name,
            vg.get_size(),
            vg.get_free_extents(),
            vg.get_extent_size(),
        );
        // TODO: Why does this magic number work but using the entire size doesn't?
        let lv = vg.create_lv_linear(&lv_name, vg.get_size() - 10_485_760)?;

        self.create_lvm_tags(
            &lv,
            &lv_dev_name,
            &osd_fsid,
            new_osd_id,
            &info,
            journal_device,
        )?;
        Ok((lv_dev_name.to_path_buf(), vg.get_size()))
    }

    // Add the lvm tags that ceph requires to identify the osd
    fn create_lvm_tags(
        &self,
        lv: &LogicalVolume<'_, '_>,
        lv_dev_name: &Path,
        osd_fsid: &uuid::Uuid,
        new_osd_id: u64,
        info: &block_utils::Device,
        journal_device: Option<&JournalDevice>,
    ) -> BynarResult<()> {
        debug!("Creating lvm tags");
        let mut tags = vec![
            format!("ceph.type={}", "block"),
            format!("ceph.block_device={}", lv_dev_name.display()),
            format!("ceph.osd_id={}", new_osd_id),
            format!("ceph.osd_fsid={}", osd_fsid),
            // TODO: Find out where to find this.
            format!("ceph.cluster_name={}", "ceph"),
            format!("ceph.cluster_fsid={}", self.cluster_handle.rados_fsid()?),
            format!("ceph.encrypted={}", "0"),
            "ceph.cephx_lockbox_secret=".to_string(),
            format!("ceph.block_uuid={}", lv.get_uuid()),
        ];
        if let Some(journal_dev) = journal_device {
            tags.push(format!("ceph.wal_device={}", journal_dev));
            let uuid = match journal_dev.partition_uuid {
                Some(uuid) => uuid,
                None => {
                    debug!("Discovering {} partition uuid", journal_dev);
                    let blkid = BlkId::new(&Path::new(&format!("{}", journal_dev)))?;
                    blkid.do_probe()?;
                    uuid::Uuid::from_str(&blkid.lookup_value("PARTUUID")?)?
                }
            };
            // Get the partition uuid from the device
            tags.push(format!("ceph.wal_uuid={}", uuid));
        }

        // Tell ceph what type of underlying media this is
        match info.media_type {
            block_utils::MediaType::SolidState => {
                tags.push("ceph.crush_device_class=ssd".into());
            }
            block_utils::MediaType::Rotational => {
                tags.push("ceph.crush_device_class=hdd".into());
            }
            block_utils::MediaType::NVME => {
                tags.push("ceph.crush_device_class=nvme".into());
            }
            _ => {
                tags.push("ceph.crush_device_class=None".into());
            }
        };

        // Add all the tags to the lvm
        debug!("Adding tags {:?} to logical volume", tags);
        for t in tags {
            lv.add_tag(&t)?;
        }
        Ok(())
    }

    fn remove_bluestore_osd(&self, dev_path: &Path, simulate: bool) -> BynarResult<()> {
        debug!("initializing LVM");
        let lvm = Lvm::new(None)?;
        lvm.scan()?;
        // Get the volume group that this device is associated with
        let vol_group_name = match lvm
            .vg_name_from_device(&dev_path.to_string_lossy())?
            .ok_or_else(|| {
                BynarError::new(format!(
                    "No volume group associated with block device: {}",
                    dev_path.display()
                ))
            }) {
            Ok(vg_group) => vg_group,
            Err(e) => {
                // This might be a filestore osd.  Fall back possibly
                if is_filestore(&dev_path)? {
                    self.remove_filestore_osd(dev_path, simulate)?;
                    return Ok(());
                } else {
                    return Err(e);
                }
            }
        };
        debug!("Found volume group: {}", vol_group_name);
        let vg = lvm.vg_open(&vol_group_name, &OpenMode::Write)?;
        // Find the logical volume in that vol group
        let lvs = vg.list_lvs()?;
        // List the tags to get the osd id
        let mut osd_id = None;
        let mut osd_fsid = None;
        for lv in &lvs {
            let tags = lv.get_tags()?;
            debug!("Found tags for logical volume: {:?}", tags);
            let id_tag = tags.iter().find(|t| t.starts_with("ceph.osd_id"));
            if let Some(tag) = id_tag {
                let parts: Vec<String> = tag.split('=').map(|s| s.to_string()).collect();
                if let Some(s) = parts.get(1) {
                    osd_id = Some(u64::from_str(s)?);
                }
            }
            let fsid_tag = tags.iter().find(|t| t.starts_with("ceph.osd_fsid"));
            if let Some(tag) = fsid_tag {
                let parts: Vec<String> = tag.split('=').map(|s| s.to_string()).collect();
                if let Some(s) = parts.get(1) {
                    osd_fsid = Some(uuid::Uuid::parse_str(s)?);
                }
            }
        }
        if osd_id.is_none() || osd_fsid.is_none() {
            return Err(BynarError::new(format!(
                "No osd id's or fsid's were found on {}",
                dev_path.display()
            )));
        }
        let osd_id = osd_id.unwrap();
        debug!("Setting osd {} out", osd_id);
        osd_out(&self.cluster_handle, osd_id, simulate)?;
        debug!("Removing osd {} from crush", osd_id);
        osd_crush_remove(&self.cluster_handle, osd_id, simulate)?;
        debug!("Deleting osd {} auth key", osd_id);
        auth_del(&self.cluster_handle, osd_id, simulate)?;
        systemctl_stop(osd_id, simulate)?;
        debug!("Removing osd {}", osd_id);
        osd_rm(&self.cluster_handle, osd_id, simulate)?;

        // Wipe the disk
        debug!("Erasing disk {}", dev_path.display());
        if !simulate {
            // Remove all logical volumes associated with this volume group
            for lv in &lvs {
                lv.deactivate()?;
                lv.remove()?;
            }
            // Remove the volume group
            vg.remove()?;
            // Remove the physical volume
            lvm.pv_remove(&dev_path.to_string_lossy())?;

            // Erase the physical volume
            match block_utils::erase_block_device(&dev_path) {
                Ok(_) => {
                    debug!("{} erased", dev_path.display());
                }
                Err(e) => {
                    // At this point the disk is about to be replaced anyways
                    // so this doesn't really matter
                    error!("{} failed to erase: {:?}", dev_path.display(), e);
                }
            };
            debug!("Cleaning up /var/lib/ceph/osd/ceph-{}", osd_id);
            remove_dir_all(Path::new("/var/lib/ceph/osd/").join(&format!("ceph-{}", osd_id)))?;
        }

        systemctl_disable(osd_id, &osd_fsid.unwrap(), simulate)?;

        Ok(())
    }

    fn remove_filestore_osd(&self, dev_path: &Path, simulate: bool) -> BynarResult<()> {
        //If the OSD is still running we can query its version.  If not then we
        //should ask either another OSD or a monitor.
        let mount_point = match block_utils::get_mountpoint(&dev_path)? {
            Some(osd_path) => osd_path,
            None => {
                let temp_dir = TempDir::new("osd")?;
                temp_dir.into_path()
            }
        };
        debug!("OSD mounted at: {:?}", mount_point);

        let osd_id = match get_osd_id(&mount_point, simulate) {
            Ok(osd_id) => osd_id,
            Err(e) => {
                error!(
                    "Failed to discover osd id: {:?}.  Falling back on path name",
                    e
                );
                get_osd_id_from_path(&mount_point)?
            }
        };
        debug!("Setting osd {} out", osd_id);
        osd_out(&self.cluster_handle, osd_id, simulate)?;
        debug!("Removing osd {} from crush", osd_id);
        osd_crush_remove(&self.cluster_handle, osd_id, simulate)?;
        debug!("Deleting osd {} auth key", osd_id);
        auth_del(&self.cluster_handle, osd_id, simulate)?;
        debug!("Removing osd {}", osd_id);
        osd_rm(&self.cluster_handle, osd_id, simulate)?;

        // Wipe the disk
        debug!("Erasing disk {}", dev_path.display());
        if !simulate {
            match block_utils::erase_block_device(&dev_path) {
                Ok(_) => {
                    debug!("{} erased", dev_path.display());
                }
                Err(e) => {
                    // At this point the disk is about to be replaced anyways
                    // so this doesn't really matter
                    error!("{} failed to erase: {:?}", dev_path.display(), e);
                }
            };
        }

        Ok(())
    }

    // lvm devices are symlinks.  They need to be resolved back into an
    // absolute path to do anything useful with them.
    fn resolve_lvm_device(&self, lv_dev_name: &Path) -> BynarResult<PathBuf> {
        debug!("Resolving lvm {} device", lv_dev_name.display());
        let tmp = lv_dev_name.read_link()?;
        if tmp.is_relative() {
            let p = lv_dev_name
                .parent()
                .ok_or_else(|| {
                    BynarError::new(format!(
                        "LVM device {} has no parent directory",
                        lv_dev_name.display()
                    ))
                })?
                .join(tmp)
                .canonicalize()?;
            Ok(p)
        } else {
            Ok(tmp)
        }
    }

    // Find the journal device that has enough free space
    fn select_journal(&self) -> BynarResult<Option<JournalDevice>> {
        let journal_size = u64::from_str(&self.cluster_handle.config_get("osd_journal_size")?)?;
        // The config file uses MB as the journal size
        let journal_size_mb = journal_size * 1024 * 1024;
        let mut journal_devices = self
            .config
            .journal_devices
            .clone()
            .unwrap_or_else(|| vec![]);
        // Sort by number of partitions
        journal_devices.sort_by_key(|j| j.num_partitions);
        // Clear any space that we can
        //remove_unused_journals(&journal_devices)?;
        let journal: Option<&JournalDevice> = journal_devices
            .iter()
            // Remove any devices without enough free space
            .filter(|d| match enough_free_space(&d.device, journal_size_mb) {
                Ok(enough) => enough,
                Err(e) => {
                    error!(
                        "Finding free space on {} failed: {:?}",
                        d.device.display(),
                        e
                    );
                    false
                }
            })
            // Take the first one
            .take(1)
            .next();
        match journal {
            Some(ref j) => Ok(Some(evaluate_journal(j, journal_size_mb)?)),
            None => Ok(None),
        }
    }
}

impl Backend for CephBackend {
    fn add_disk(&self, device: &Path, id: Option<u64>, simulate: bool) -> BynarResult<()> {
        debug!("ceph version: {:?}", self.version,);
        if self.version >= CephVersion::Luminous {
            self.add_bluestore_osd(device, id, simulate)?;
        } else {
            self.add_filestore_osd(device, id, simulate)?;
        }
        Ok(())
    }

    fn remove_disk(&self, device: &Path, simulate: bool) -> BynarResult<()> {
        if self.version >= CephVersion::Luminous {
            // Check if the type file exists
            self.remove_bluestore_osd(device, simulate)?;
        } else {
            self.remove_filestore_osd(device, simulate)?;
        }
        Ok(())
    }

    fn safe_to_remove(&self, _device: &Path, _simulate: bool) -> BynarResult<bool> {
        let diag_map = DiagMap::new().map_err(|e| BynarError::new(e.to_string()))?;
        debug!("Checking if a disk is safe to remove from ceph");
        match diag_map.exhaustive_diag(Format::Json) {
            Status::Safe => Ok(true),
            Status::NonSafe => Ok(false),
            Status::Unknown => Ok(false),
        }
    }
}

// A fallback function to get the osd id from the mount path.  This isn't
// 100% accurate but it should be good enough for most cases unless the disk
// is mounted in the wrong location or is missing an osd id in the path name
fn get_osd_id_from_path(path: &Path) -> BynarResult<u64> {
    match path.file_name() {
        Some(name) => {
            let name_string = name.to_string_lossy().into_owned();
            let parts: Vec<&str> = name_string.split('-').collect();
            let id = u64::from_str(parts[1])?;
            Ok(id)
        }
        None => Err(BynarError::new(format!(
            "Unable to get filename from {}",
            path.display()
        ))),
    }
}

// Get an osd ID from the whoami file in the osd mount directory
fn get_osd_id(path: &Path, simulate: bool) -> BynarResult<u64> {
    if simulate {
        return Ok(0);
    }
    let whoami_path = path.join("whoami");
    debug!("Discovering osd id number from: {}", whoami_path.display());
    let buff = read_to_string(&whoami_path)?;
    Ok(u64::from_str(buff.trim())?)
}

fn save_keyring(
    osd_id: u64,
    key: &str,
    uid: Option<u32>,
    gid: Option<u32>,
    simulate: bool,
) -> BynarResult<()> {
    let uid = uid.and_then(|u| Some(Uid::from_raw(u)));
    let gid = gid.and_then(|g| Some(Gid::from_raw(g)));
    let base_dir = Path::new("/var/lib/ceph/osd").join(&format!("ceph-{}", osd_id));
    if !Path::new(&base_dir).exists() {
        return Err(BynarError::new(format!(
            "{} directory doesn't exist",
            base_dir.display()
        )));
    }
    debug!("Creating {}/keyring", base_dir.display());
    if !simulate {
        let mut f = File::create(base_dir.join("keyring"))?;
        f.write_all(format!("[osd.{}]\n\tkey = {}\n", osd_id, key).as_bytes())?;
        chown(&base_dir.join("keyring"), uid, gid)?;
    }
    Ok(())
}

fn add_osd_to_fstab(
    device_info: &block_utils::Device,
    osd_id: u64,
    simulate: bool,
) -> BynarResult<()> {
    let fstab = FsTab::default();
    let fstab_entry = fstab::FsEntry {
        fs_spec: format!(
            "UUID={}",
            device_info.id.unwrap().to_hyphenated().to_string()
        ),
        mountpoint: PathBuf::from(&format!("/var/lib/ceph/osd/ceph-{}", osd_id)),
        vfs_type: device_info.fs_type.to_string(),
        mount_options: vec![
            "noatime".into(),
            "inode64".into(),
            "attr2".into(),
            "logbsize=256k".into(),
            "noquota".into(),
        ],
        dump: false,
        fsck_order: 2,
    };
    debug!("Saving Fstab entry {:?}", fstab_entry);
    if !simulate {
        let result = fstab.add_entry(fstab_entry)?;
        if result {
            debug!("Fstab entry saved");
        } else {
            debug!("Fstab entry was updated");
        }
    }
    Ok(())
}

// Look through all the /var/lib/ceph/osd/ directories and try to find
// a partition id that matches this one.
fn partition_in_use(partition_uuid: &uuid::Uuid) -> BynarResult<bool> {
    // Check every osd on the system
    for osd_dir in read_dir("/var/lib/ceph/osd/")? {
        let osd_dir = osd_dir?;
        trace!("Locating journal symlink in {}", osd_dir.path().display());
        // Ceph Jewel and older uses journal as the journal symlink name
        let old_journal_path = osd_dir.path().join("journal");
        // Ceph Luminous and newer users block.wal as the journal device symlink name
        let new_journal_path = osd_dir.path().join("block.wal");

        let journal_path = match (old_journal_path.exists(), new_journal_path.exists()) {
            (true, true) => {
                // Ok this isn't possible
                return Err(BynarError::new(format!(
                    "Unable to determine which journal path to use.  Both {} and {} exist.",
                    old_journal_path.display(),
                    new_journal_path.display(),
                )));
            }
            (true, false) => {
                // Old Ceph
                old_journal_path
            }
            (false, true) => {
                // New Ceph
                new_journal_path
            }
            (false, false) => {
                // No journal
                return Ok(false);
            }
        };
        debug!("Journal path: {}", journal_path.display());
        let meta = symlink_metadata(&journal_path)?;
        if !meta.file_type().is_symlink() {
            // Whoops.  Symlink pointer missing.  Can't proceed
            // TODO: Is this always true?
            return Err(BynarError::new(format!(
                "Journal {} is not a symlink. Unable to find the device this journal points to",
                journal_path.display(),
            )));
        }

        // Resolve the device the symlink points to
        let dev = journal_path.read_link()?;
        let blkid = BlkId::new(&dev)?;
        blkid.do_probe()?;
        // Get the partition uuid from the device
        let dev_partition_uuid = uuid::Uuid::from_str(&blkid.lookup_value("PARTUUID")?)?;
        debug!("Journal partition uuid: {}", dev_partition_uuid);
        if partition_uuid == &dev_partition_uuid {
            return Ok(true);
        }
    }

    Ok(false)
}

fn systemctl_disable(osd_id: u64, osd_uuid: &uuid::Uuid, simulate: bool) -> BynarResult<()> {
    if !simulate {
        let args: Vec<String> = vec![
            "disable".to_string(),
            format!("ceph-volume@lvm-{}-{}", osd_id, osd_uuid.to_hyphenated()),
        ];
        debug!("cmd: systemctl {:?}", args);
        let output = Command::new("systemctl").args(&args).output()?;
        if !output.status.success() {
            return Err(BynarError::new(
                String::from_utf8_lossy(&output.stderr).into_owned(),
            ));
        }
    }
    Ok(())
}

fn systemctl_enable(osd_id: u64, osd_uuid: &uuid::Uuid, simulate: bool) -> BynarResult<()> {
    if !simulate {
        let args: Vec<String> = vec![
            "enable".to_string(),
            format!("ceph-volume@lvm-{}-{}", osd_id, osd_uuid.to_hyphenated()),
        ];
        debug!("cmd: systemctl {:?}", args);
        let output = Command::new("systemctl").args(&args).output()?;
        if !output.status.success() {
            return Err(BynarError::new(
                String::from_utf8_lossy(&output.stderr).into_owned(),
            ));
        }
    }
    Ok(())
}

fn systemctl_stop(osd_id: u64, simulate: bool) -> BynarResult<()> {
    if !simulate {
        let args: Vec<String> = vec!["stop".to_string(), format!("ceph-osd@{}.service", osd_id)];
        debug!("cmd: systemctl {:?}", args);
        let output = Command::new("systemctl").args(&args).output()?;
        if !output.status.success() {
            return Err(BynarError::new(
                String::from_utf8_lossy(&output.stderr).into_owned(),
            ));
        }
    }
    Ok(())
}

fn setup_osd_init(osd_id: u64, simulate: bool) -> BynarResult<()> {
    debug!("Detecting init system");
    let init_daemon = detect_daemon()?;
    match init_daemon {
        Daemon::Systemd => {
            debug!("Systemd detected.  Starting OSD");
            let mut cmd = Command::new("systemctl");
            cmd.arg("start");
            cmd.arg(format!("ceph-osd@{}", osd_id));
            debug!("cmd: {:?}", cmd);
            if !simulate {
                let output = cmd.output()?;
                if !output.status.success() {
                    return Err(BynarError::new(
                        String::from_utf8_lossy(&output.stderr).into_owned(),
                    ));
                }
            }
            Ok(())
        }
        Daemon::Upstart => {
            debug!("Upstart detected.  Starting OSD");
            let mut cmd = Command::new("start");
            cmd.arg("ceph-osd");
            cmd.arg(format!("id={}", osd_id));
            debug!("cmd: {:?}", cmd);
            if !simulate {
                let output = cmd.output()?;
                if !output.status.success() {
                    return Err(BynarError::new(
                        String::from_utf8_lossy(&output.stderr).into_owned(),
                    ));
                }
            }
            Ok(())
        }
        Daemon::Unknown => Err(BynarError::from(
            "Unknown init system.  Cannot start osd service",
        )),
    }
}

fn settle_udev() -> BynarResult<()> {
    let output = Command::new("udevadm").arg("settle").output()?;
    if !output.status.success() {
        return Err(BynarError::new(
            String::from_utf8_lossy(&output.stderr).into_owned(),
        ));
    }
    Ok(())
}

// Run ceph-osd --mkfs and return the osd UUID
fn ceph_mkfs(
    osd_id: u64,
    journal: Option<&JournalDevice>,
    bluestore: bool,
    monmap: Option<&Path>,
    osd_data: Option<&Path>,
    osd_uuid: Option<&uuid::Uuid>,
    user_id: Option<&str>,
    group_id: Option<&str>,
    simulate: bool,
) -> BynarResult<()> {
    debug!("Running ceph-osd --mkfs");
    let mut args: Vec<String> = vec![
        "--cluster".to_string(),
        "ceph".to_string(),
        "-i".to_string(),
        osd_id.to_string(),
        "--mkfs".to_string(),
    ];
    if let Some(journal) = journal {
        args.push("--osd-journal".to_string());
        args.push(format!("{}", journal));
    }
    if bluestore {
        args.extend_from_slice(&["--osd-objectstore".to_string(), "bluestore".to_string()]);
    }
    if let Some(monmap) = monmap {
        args.push("--monmap".to_string());
        args.push(monmap.to_string_lossy().into_owned());
    }
    if let Some(osd_data) = osd_data {
        args.push("--osd-data".to_string());
        args.push(osd_data.to_string_lossy().into_owned());
    }
    if let Some(osd_uuid) = osd_uuid {
        args.push("--osd-uuid".to_string());
        args.push(osd_uuid.to_hyphenated().to_string());
    }
    if let Some(u_id) = user_id {
        args.push("--setuser".to_string());
        args.push(u_id.to_string());
    }
    if let Some(g_id) = group_id {
        args.push("--setgroup".to_string());
        args.push(g_id.to_string());
    }

    debug!("cmd: ceph-osd {:?}", args);
    if simulate {
        return Ok(());
    }
    let output = Command::new("ceph-osd").args(&args).output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        error!(
            "ceph-osd cmd failed: {}. stderr: {}",
            String::from_utf8_lossy(&output.stdout),
            stderr
        );
        return Err(BynarError::new(stderr));
    }
    Ok(())
}

fn ceph_bluestore_tool(device: &Path, mount_path: &Path, simulate: bool) -> BynarResult<()> {
    let dev_str = device.to_string_lossy().into_owned();
    let mnt_str = mount_path.to_string_lossy().into_owned();
    let mut args: Vec<&str> = vec!["--cluster=ceph", "prime-osd-dir"];

    args.push("--dev");
    args.push(&dev_str);
    args.push("--path");
    args.push(&mnt_str);

    debug!("cmd: ceph-bluestore-tool {:?}", args);
    if simulate {
        return Ok(());
    }

    let output = Command::new("ceph-bluestore-tool").args(&args).output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        error!(
            "ceph-bluestore-tool cmd failed: {}. stderr: {}",
            String::from_utf8_lossy(&output.stdout),
            stderr
        );
        return Err(BynarError::new(stderr));
    }
    Ok(())
}

/// Create a new ceph journal on a given deivce with name + size in bytes
fn create_journal(name: &str, size: u64, path: &Path) -> BynarResult<(u32, uuid::Uuid)> {
    debug!("Creating journal on {} of size: {}", path.display(), size);
    let cfg = gpt::GptConfig::new().writable(true).initialized(true);
    let mut disk = cfg.open(path)?;
    let part_id = disk.add_partition(name, size, gpt::partition_types::CEPH_JOURNAL, 0)?;
    // Write it out
    disk.write()?;
    update_partition_cache(&path)?;

    // Read it back in
    let cfg = gpt::GptConfig::new().writable(false).initialized(true);
    let disk = cfg.open(path)?;
    let partition = {
        let part = disk.partitions().get(&part_id);
        match part {
            Some(part) => part,
            None => {
                return Err(BynarError::new(format!(
                    "Added partition {} to {} but partition not found",
                    part_id,
                    path.display()
                )));
            }
        }
    };

    Ok((part_id, partition.part_guid))
}

// Returns true if there's enough free space on the disk to fit a given
// partition size request.
fn enough_free_space(device: &Path, size: u64) -> BynarResult<bool> {
    let cfg = gpt::GptConfig::new().writable(false).initialized(true);
    let disk = cfg.open(device)?;
    let free_spots = disk.find_free_sectors();
    for (_, length_lba) in free_spots {
        let lba_size: u64 = match disk.logical_block_size() {
            gpt::disk::LogicalBlockSize::Lb512 => 512,
            gpt::disk::LogicalBlockSize::Lb4096 => 4096,
        };
        if (length_lba * lba_size) > size {
            return Ok(true);
        }
    }

    Ok(false)
}

// A JournalDevice and size is given and this function will:
// 1. Attempt to discover if a device exists at that journal path
// 2. Create a journal partition if needed.
// 3. Returns a path to use for the journal
fn evaluate_journal(journal: &JournalDevice, journal_size: u64) -> BynarResult<JournalDevice> {
    match (&journal.device, journal.partition_id) {
        (journal, Some(part_id)) => {
            // Got both a journal device and a partition id
            // Check if it exists and whether it's in use by another osd
            let cfg = gpt::GptConfig::new().writable(false).initialized(true);
            let disk = cfg.open(&journal)?;
            //Locate the partition the user requested to use
            for partition in disk.partitions() {
                if partition.0 == &part_id {
                    // How do we know if another ceph osd is using this partition?
                    // Check all other osds for this partition_id
                    if !partition_in_use(&partition.1.part_guid)? {
                        // It's ok to use this
                        return Ok(JournalDevice {
                            device: journal.to_path_buf(),
                            partition_id: Some(part_id),
                            partition_uuid: None,
                            num_partitions: Some(1),
                        });
                    } else {
                        // Create a new partition because the old one is in use
                        let partition_info =
                            create_journal("ceph_journal", journal_size, &journal)?;
                        let mut j = JournalDevice {
                            device: journal.to_path_buf(),
                            partition_id: Some(partition_info.0),
                            partition_uuid: Some(partition_info.1),
                            num_partitions: None,
                        };
                        j.update_num_partitions()?;
                        return Ok(j);
                    }
                }
            }
            // User has asked to use a particular device but we can't find it
            Err(BynarError::new(format!(
                "{}{} not found for journal device",
                journal.display(),
                part_id
            )))
        }
        (journal, None) => {
            // Got just a journal device
            // Create a new journal partition on there
            let partition_info = create_journal("ceph_journal", journal_size, &journal)?;
            let mut j = JournalDevice {
                device: journal.to_path_buf(),
                partition_id: Some(partition_info.0),
                partition_uuid: Some(partition_info.1),
                num_partitions: None,
            };
            j.update_num_partitions()?;
            Ok(j)
        }
    }
}

// NOTE: This function is currently unused because I don't have complete trust
// in it yet.
// Checks all osd drives on the system against the journals and deletes all
// unused partitions.
fn remove_unused_journals(journals: &[JournalDevice]) -> BynarResult<()> {
    for journal in journals {
        let cfg = gpt::GptConfig::new().writable(true).initialized(true);
        debug!("Checking for unused journal partitions on {}", journal);
        let mut disk = cfg.open(&journal.device)?;
        let mut changed = false;
        let mut partitions: BTreeMap<u32, gpt::partition::Partition> = disk.partitions().clone();
        for part in partitions.iter_mut() {
            trace!("Checking if {:?} is in use", part);
            let partition_used = match partition_in_use(&part.1.part_guid) {
                Ok(used) => used,
                Err(e) => {
                    error!("partition_in_use error: {:?}. Not modifying partition", e);
                    true
                }
            };
            if !partition_used {
                // mark as unused
                changed = true;
                part.1.part_type_guid = gpt::partition_types::UNUSED;
            }
        }
        if changed {
            trace!("Saving partitions: {:?}", partitions);
            disk.update_partitions(partitions)?;
            disk.write()?;
        }
    }

    Ok(())
}

fn is_filestore(dev_path: &Path) -> BynarResult<bool> {
    let mount_point = match block_utils::get_mountpoint(&dev_path)? {
        Some(osd_path) => osd_path,
        None => {
            let tmp_dir = TempDir::new("osd")?;
            let tmp_path = tmp_dir.into_path();
            let dev_info = block_utils::get_device_info(&dev_path)?;
            block_utils::mount_device(&dev_info, &tmp_path)?;
            tmp_path
        }
    };
    debug!("OSD mounted at: {:?}", mount_point);
    let type_path = mount_point.join("type");
    if type_path.exists() {
        let osd_type_contents = read_to_string(&type_path)?;
        if osd_type_contents.trim() == "filestore" {
            return Ok(true);
        }
    }

    Ok(false)
}

// Linux specific ioctl to update the partition table cache.
fn update_partition_cache(device: &Path) -> BynarResult<()> {
    debug!(
        "Requesting kernel to refresh partition cache for {} ",
        device.display()
    );
    let device = OpenOptions::new().read(true).write(true).open(device)?;
    let ret = unsafe { blkrrpart(device.as_raw_fd()) }?;
    if ret != 0 {
        Err(BynarError::new(format!(
            "BLKRRPART ioctl failed with return code: {}",
            ret,
        )))
    } else {
        Ok(())
    }
}

// This macro from the nix crate crates an ioctl to call the linux kernel
// and ask it to update its internal partition cache. Without this the
// partitions don't show up after being created on the disks which then
// breaks parts of bynar later.
ioctl_none! {
    /// Linux BLKRRPART ioctl to update partition tables.  Defined in linux/fs.h
    blkrrpart, 0x12, 95
}
