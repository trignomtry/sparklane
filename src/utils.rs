use crate::*;
use std::io::Cursor;
use zip::ZipArchive;

#[derive(Clone, Copy)]
pub struct Db {}

impl Db {
    pub async fn get(self, k: &str) -> Result<Option<Vec<u8>>, &'static str> {
        let v = k.as_bytes();
        let db = foundationdb::Database::default().map_err(|_e| "Database get error")?;
        match db
            .run(|trx, _maybe_committed| async move {
                let tr = match trx.get(v, false).await {
                    Ok(tr) => tr,
                    Err(e) => {
                        eprintln!("Error commiting transaction: {e}");
                        return Err(foundationdb::FdbBindingError::ReferenceToTransactionKept);
                    }
                };
                Ok(tr)
            })
            .await
        {
            Ok(slice) => Ok(slice.map(move |e| e.as_ref().to_vec())),
            Err(e) => {
                eprintln!("Commit transaction error: {e}");
                Err("cannot commit transaction")
            }
        }
    }
    pub async fn insert(self, k: &str, v: &[u8]) -> Result<(), &'static str> {
        let key: &[u8] = k.as_bytes();
        let db = foundationdb::Database::default().map_err(|_e| "Database insert error")?;
        db.run(async move |trx, _maybe_committed| {
            trx.set(key, v);
            Ok(())
        })
        .await
        .map_err(|e| {
            eprintln!("Cannot commit transaction in insert: {:?}", e);
            "Database insert transaction failed"
        })
    }
    pub async fn scan_prefix(self, prefix: &str) -> Result<Vec<(Vec<u8>, Vec<u8>)>, &'static str> {
        let mut end = prefix.as_bytes().to_vec();
        end.push(0xFF);
        let ran = foundationdb::RangeOption::from((prefix.as_ref(), end.as_slice()));
        let db = foundationdb::Database::default().map_err(|_e| "Database scan_prefix error")?;
        match db
            .run(|trx, _maybe_committed| {
                let ran = ran.clone();
                async move { Ok(trx.get_range(&ran, 100000000, false).await) }
            })
            .await
        {
            Ok(r) => Ok(r
                .map_err(|_| "Scan prefix value error")?
                .iter()
                .map(move |v| (v.key().to_vec(), v.value().to_vec()))
                .collect()),
            Err(_) => Err("Scan prefix error"),
        }
    }
    pub async fn remove(self, k: &str) -> Result<(), &'static str> {
        let db = foundationdb::Database::default().map_err(|_e| "Database scan_prefix error")?;
        match db
            .run(|trx, _maybe_committed| {
                let key = k.as_bytes();
                async move {
                    trx.clear(key);
                    Ok(())
                }
            })
            .await
        {
            Ok(r) => Ok(r),
            Err(e) => {
                eprintln!("Db removal error: {e}");
                Err("Db removal error")
            }
        }
    }
}

pub fn extract_zip_to_vec(zip_bytes: &[u8]) -> std::io::Result<Vec<(String, Vec<u8>)>> {
    let cursor = Cursor::new(zip_bytes);
    let mut archive = ZipArchive::new(cursor)?;
    let mut files = Vec::new();

    for i in 0..archive.len() {
        let mut file = archive.by_index(i)?;
        if file.is_dir() {
            continue;
        }

        let mut contents = Vec::new();
        std::io::copy(&mut file, &mut contents)?;
        let path = file.name().to_string();
        files.push((path, contents));
    }

    Ok(files)
}

pub async fn spin(cfg: &Config, user_js: Vec<(String, Vec<u8>)>) -> std::io::Result<()> {
    let base_img_path = "/mnt/sparklane/base.img";
    let vm_img_path = format!("/mnt/vm-images/{}.img", cfg.id);
    let mount_dir = format!("/mnt/vm-usercode-{}", cfg.id);
    let app_path = format!("{}/app", &mount_dir);

    // Step 1: Copy base.img to new image
    std::fs::create_dir_all("/mnt/vm-images")?;
    std::fs::copy(base_img_path, &vm_img_path)?;

    // Step 2: Mount the image
    fs::create_dir_all(&mount_dir).await?;
    Command::new("mount")
        .args(["-o", "loop", &vm_img_path, &mount_dir])
        .status()?;
    fs::create_dir_all(&app_path).await?;

    let db = Db {};

    // Check if instance already exists and insert if not
    match db.get(&format!("vm:{}", &cfg.id)).await {
        Ok(Some(_)) => {
            eprintln!("Instance {} already exists.", cfg.id);
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                format!("Instance {} already exists.", cfg.id),
            ));
        }
        Ok(None) => {
            if let Err(db_err_msg) = db
                .insert(
                    &format!("instance:{}", cfg.id),
                    serde_json::to_string(cfg).unwrap().as_bytes(),
                )
                .await
            {
                eprintln!(
                    "Failed to insert instance record for {}: {}",
                    cfg.id, db_err_msg
                );
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!(
                        "Database operation failed during instance creation: {}",
                        db_err_msg
                    ),
                ));
            }
        }
        Err(db_err_msg) => {
            eprintln!("Failed to query instance {}: {}", cfg.id, db_err_msg);
            return Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!(
                    "Database operation failed while checking instance: {}",
                    db_err_msg
                ),
            ));
        }
    }

    for (path, js) in user_js {
        fs::write(format!("{}/{}", &app_path, path), js).await?;
    }

    // 6. Write init.sh
    let init = format!(
        "#!/bin/bash\ncd /app\n{}\n{}\npoweroff -f",
        cfg.build_commands.join("\n"),
        cfg.run_command
    );
    let config_json = format!(
        r#"
    {{
  "boot-source": {{
    "kernel_image_path": "/mnt/vmlinux",    
    "boot_args": "console=ttyS0 reboot=k panic=1 pci=off"
    }},
  "drives": [
    {{
      "drive_id": "rootfs",
      "path_on_host": "{}",
      "is_root_device": true,
      "is_read_only": false
    }}
  ],
  "network-interfaces": [
    {{
      "iface_id": "eth0",
      "host_dev_name": "tap{}",
      "guest_mac": "{}"
    }}
  ]
  ,
  "console-cfg": {{
    "file": "/tmp/firecracker-{}.log"
  }}
}}

    "#,
        &vm_img_path,
        &cfg.id[..8],
        generate_mac(&cfg.id),
        cfg.id
    );
    let config_path = format!("/tmp/vm-{}.json", cfg.id);
    // Ensure /tmp exists, though it usually does
    fs::write(&config_path, config_json).await?;

    fs::write(format!("{}/../init.sh", &app_path), init).await?;
    fs::set_permissions(
        format!("{}/../init.sh", &app_path),
        std::fs::Permissions::from_mode(0o755),
    )
    .await?;

    // 7. Symlink /init
    let symlink_path = format!("{}/init", &mount_dir);
    if std::path::Path::new(&symlink_path).exists() {
        fs::remove_file(&symlink_path).await?;
    }

    std::os::unix::fs::symlink("init.sh", &symlink_path)?;

    Command::new("ip")
        .args([
            "tuntap",
            "add",
            "mode",
            "tap",
            &format!("tap{}", &cfg.id[..8]),
        ])
        .status()?;

    Command::new("ip")
        .args([
            "tuntap",
            "add",
            "mode",
            "tap",
            &format!("tap{}", &cfg.id[..8]),
        ])
        .status()?;

    Command::new("ip")
        .args(["link", "set", &format!("tap{}", &cfg.id[..8]), "up"])
        .status()?;

    let _ = fs::remove_file(format!("/tmp/firecracker-{}.sock", cfg.id)).await;
    println!("Crackin...");
    // Execute the firecracker command and wait for it to finish
    let firecracker_status = Command::new("firecracker")
        .args([
            "--api-sock",
            &format!("/tmp/firecracker-{}.sock", cfg.id),
            "--config-file",
            &config_path,
        ])
        .status()?;

    if !firecracker_status.success() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!("Firecracker command failed for VM {}", cfg.id),
        ));
    }
    Ok(())
}

fn unspin(id: &str) -> std::io::Result<()> {
    Command::new("umount")
        .arg(format!("/mnt/vm-usercode-{}", id))
        .status()?;
    Ok(())
}

fn generate_mac(id: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut hasher = DefaultHasher::new();
    id.hash(&mut hasher);
    let hash = hasher.finish();

    format!(
        "AA:FC:{:02X}:{:02X}:{:02X}:{:02X}",
        (hash >> 24) & 0xFF,
        (hash >> 16) & 0xFF,
        (hash >> 8) & 0xFF,
        hash & 0xFF
    )
}
