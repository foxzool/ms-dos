//! web 端瓦片持久缓存（CacheStorage，wasm 专用）。
//!
//! 桌面端走磁盘缓存（`~/.cache/ms-dos/tiles/`）；web 端此前每次会话重新下载。
//! 此处以 CacheStorage（Service Worker 同款存储，跨会话持久，浏览器配额自动驱逐）
//! 承载瓦片字节。键使用合成 URL（`https://msdos.tiles/<source>/z/y/x`）而非原始请求
//! URL——OpenFreeMap 的瓦片模板带滚动的 build 路径，原始 URL 作键会在其更新后全部
//! miss 并留下死条目；合成键与 z/x/y 一一对应，跨模板更新稳定命中。
//! 瓦片内容基本不可变（GIBS 静态底图 / OSM 数据月级更新），不设 TTL；
//! 缓存名带版本号，需要失效时递增即可。

/// 缓存版本（内容格式变化时递增以整体失效）
#[cfg(target_arch = "wasm32")]
const CACHE_NAME: &str = "msdos-tiles-v2";

/// 先查 CacheStorage，miss 则网络请求并写回。缓存读写失败一律容忍（回退纯网络）。
/// 仅 web 构建存在；桌面端调用点走磁盘缓存分支。
#[cfg(target_arch = "wasm32")]
pub async fn cached_fetch(url: &str, cache_key: &str) -> Result<Vec<u8>, String> {
    cached_fetch_if(url, cache_key, |_| true).await
}

/// 带内容校验的缓存获取：校验失败的响应不写缓存（防止限流期的 200 损坏数据被永久缓存）
#[cfg(target_arch = "wasm32")]
pub async fn cached_fetch_if(
    url: &str,
    cache_key: &str,
    valid: impl Fn(&[u8]) -> bool,
) -> Result<Vec<u8>, String> {
    use wasm_bindgen::JsCast;
    use wasm_bindgen_futures::JsFuture;

    let window = web_sys::window().ok_or("无 window 对象")?;
    let caches = window.caches().map_err(|_| "无 CacheStorage")?;

    // ---- 查缓存 ----
    {
        let cache_promise = caches.open(CACHE_NAME);
        if let Ok(cache_val) = JsFuture::from(cache_promise).await {
            if let Ok(cache) = cache_val.dyn_into::<web_sys::Cache>() {
                if let Ok(hit_val) = JsFuture::from(cache.match_with_str(cache_key)).await {
                    // undefined = miss
                    if !hit_val.is_undefined() {
                        if let Ok(resp) = hit_val.dyn_into::<web_sys::Response>() {
                            if let Ok(buf) =
                                JsFuture::from(resp.array_buffer().map_err(|e| format!("{e:?}"))?).await
                            {
                                let bytes = js_sys::Uint8Array::new(&buf).to_vec();
                                if !bytes.is_empty() {
                                    return Ok(bytes);
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // ---- 网络请求 ----
    let resp_val = JsFuture::from(window.fetch_with_str(url))
        .await
        .map_err(|e| format!("网络失败: {e:?}"))?;
    let resp: web_sys::Response = resp_val.dyn_into().map_err(|_| "响应类型异常")?;
    if !resp.ok() {
        return Err(format!("HTTP {}", resp.status()));
    }
    let buf = JsFuture::from(resp.array_buffer().map_err(|e| format!("{e:?}"))?)
        .await
        .map_err(|e| format!("读取失败: {e:?}"))?;
    let bytes = js_sys::Uint8Array::new(&buf).to_vec();

    // ---- 写回缓存（校验通过才写；失败容忍） ----
    if valid(&bytes) {
        let cache_promise = caches.open(CACHE_NAME);
        if let Ok(cache_val) = JsFuture::from(cache_promise).await {
            if let Ok(cache) = cache_val.dyn_into::<web_sys::Cache>() {
                let mut body = bytes.clone();
                if let Ok(stored) = web_sys::Response::new_with_opt_u8_array(Some(&mut body)) {
                    let _ = JsFuture::from(cache.put_with_str(cache_key, &stored)).await;
                }
            }
        }
    }
    Ok(bytes)
}

/// GIBS 地球贴图瓦片的稳定缓存键（z/y/x 与请求路径一致）
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
pub fn gibs_cache_key(z: u8, x: u32, y: u32) -> String {
    format!("https://msdos.tiles/gibs/{z}/{y}/{x}")
}

/// OpenFreeMap MVT 瓦片的稳定缓存键（z/x/y）
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
pub fn ofm_cache_key(z: u8, x: u32, y: u32) -> String {
    format!("https://msdos.tiles/ofm/{z}/{x}/{y}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_keys_are_stable_and_distinct() {
        // 键只依赖 z/x/y，与瓦片源滚动模板无关
        assert_eq!(gibs_cache_key(4, 9, 5), "https://msdos.tiles/gibs/4/5/9");
        assert_eq!(ofm_cache_key(10, 370, 244), "https://msdos.tiles/ofm/10/370/244");
        assert_ne!(gibs_cache_key(4, 9, 5), ofm_cache_key(4, 9, 5));
    }
}

/// 按天缓存的高频小数据（TLE）：缓存名带 UTC 日期，隔天自然失效；
/// 旧的按天缓存会在下次调用时被清理（TLE 数据每天更新，旧数据无保留价值）。
#[cfg(target_arch = "wasm32")]
pub async fn cached_fetch_daily(kind: &str, url: &str) -> Result<Vec<u8>, String> {
    use wasm_bindgen::JsCast;
    use wasm_bindgen_futures::JsFuture;

    let epoch_day = (js_sys::Date::now() / 86_400_000.0) as i64;
    let today = format!("msdos-{kind}-d{epoch_day}");
    let window = web_sys::window().ok_or("无 window 对象")?;
    let caches = window.caches().map_err(|_| "无 CacheStorage")?;

    // 清理旧日缓存（失败容忍）
    if let Ok(keys_val) = JsFuture::from(caches.keys()).await {
        let names = js_sys::Array::from(&keys_val);
        for i in 0..names.length() {
            if let Some(n) = names.get(i).as_string() {
                if n.starts_with("msdos-") && n.contains("-d") && n != today {
                    let _ = JsFuture::from(caches.delete(&n)).await;
                }
            }
        }
    }

    let key = format!("https://msdos.cache/{kind}");
    // 查当天缓存
    if let Ok(c) = JsFuture::from(caches.open(&today)).await {
        if let Ok(cache) = c.dyn_into::<web_sys::Cache>() {
            if let Ok(hit) = JsFuture::from(cache.match_with_str(&key)).await {
                if !hit.is_undefined() {
                    if let Ok(resp) = hit.dyn_into::<web_sys::Response>() {
                        if let Ok(buf) = JsFuture::from(resp.array_buffer().map_err(|e| format!("{e:?}"))?).await {
                            let bytes = js_sys::Uint8Array::new(&buf).to_vec();
                            if !bytes.is_empty() {
                                return Ok(bytes);
                            }
                        }
                    }
                }
            }
        }
    }
    // 网络
    let resp_val = JsFuture::from(window.fetch_with_str(url))
        .await
        .map_err(|e| format!("网络失败: {e:?}"))?;
    let resp: web_sys::Response = resp_val.dyn_into().map_err(|_| "响应类型异常")?;
    if !resp.ok() {
        return Err(format!("HTTP {}", resp.status()));
    }
    let buf = JsFuture::from(resp.array_buffer().map_err(|e| format!("{e:?}"))?)
        .await
        .map_err(|e| format!("读取失败: {e:?}"))?;
    let bytes = js_sys::Uint8Array::new(&buf).to_vec();
    // 写回（失败容忍）
    if let Ok(c) = JsFuture::from(caches.open(&today)).await {
        if let Ok(cache) = c.dyn_into::<web_sys::Cache>() {
            let mut body = bytes.clone();
            if let Ok(stored) = web_sys::Response::new_with_opt_u8_array(Some(&mut body)) {
                let _ = JsFuture::from(cache.put_with_str(&key, &stored)).await;
            }
        }
    }
    Ok(bytes)
}
