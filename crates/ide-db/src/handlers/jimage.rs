use base_db::path_resolver::VirtualPathHandler;
use dashmap::DashMap;
use jimage_rs::JImage;
use std::sync::Arc;

#[derive(Default)]
pub struct JimageManager {
    cache: DashMap<String, Arc<JImage>>,
}

impl JimageManager {
    pub fn get_jimage(&self, path: &str) -> std::io::Result<Arc<JImage>> {
        if let Some(img) = self.cache.get(path) {
            return Ok(img.clone());
        }

        let img = JImage::open(path)
            .map_err(|e| std::io::Error::other(format!("JImage open error: {:?}", e)))?;

        let arc_img = Arc::new(img);
        self.cache.insert(path.to_string(), arc_img.clone());
        Ok(arc_img)
    }
}

pub struct JimageHandler {
    manager: JimageManager,
}

impl JimageHandler {
    pub fn new() -> Self {
        Self {
            manager: JimageManager::default(),
        }
    }
}

impl Default for JimageHandler {
    fn default() -> Self {
        Self::new()
    }
}

impl VirtualPathHandler for JimageHandler {
    fn can_handle(&self, protocol: &str) -> bool {
        protocol == "jrt"
    }

    fn fetch_bytes(&self, path: &str) -> std::io::Result<Vec<u8>> {
        let (img_path, resource_path) = path.split_once('!').ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "Invalid JRT path, missing '!'",
            )
        })?;

        let jimage = self.manager.get_jimage(img_path)?;

        let resource_path = if !resource_path.starts_with('/') {
            format!("/{}", resource_path)
        } else {
            resource_path.to_string()
        };

        match jimage.find_resource(&resource_path) {
            Ok(Some(data)) => Ok(data.into_owned()),
            Ok(None) => Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!(
                    "Resource {} not found in jimage {}",
                    resource_path, img_path
                ),
            )),
            Err(e) => Err(std::io::Error::other(format!(
                "JImage find_resource error: {:?}",
                e
            ))),
        }
    }
}
