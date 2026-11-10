use super::*;

thread_local! {
    /// Decoded images are retained only for emoji actually seen in terminal
    /// output. A full 72x72 RGBA Twemoji is ~20 KiB; this avoids decoding on
    /// every redraw without eagerly allocating the entire emoji collection.
    ///
    /// Bounded + approximate LRU: the cache used to grow without limit, so
    /// scrolling through output full of distinct emoji (logs, chat, test
    /// output) accumulated ~20 KiB per glyph indefinitely. Over the cap, the
    /// least-recently-used half is evicted — terminal access is dominated by
    /// re-drawing the on-screen set every frame, so recency tracks the actual
    /// working set. A monotonically increasing tick stamps each hit.
    static TWEMOJI_CACHE: RefCell<HashMap<String, (Option<slint::Image>, u64)>> =
        RefCell::new(HashMap::new());
    static TWEMOJI_TICK: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Maximum decoded emoji images kept per thread (~128 x 20 KiB = ~2.5 MiB).
///
/// 512 时的上界是 ~10 MiB —— 而**全部**内嵌 twemoji 源 PNG 才 4.07 MB，等于缓存能
/// 存下比全量素材还多的解码位图（B1.6）。实测命中需求是"当前屏幕上的那几十个字形"，
/// 128 已经远超这个工作集；真正的上界问题在于**未被淘汰的 image 被 span 引用住**，
/// 那由 B1.7（切走标签页时清空其 span 模型）解决。
pub(crate) const TWEMOJI_CACHE_CAP: usize = 128;

pub(crate) fn twemoji_image(grapheme: &str) -> Option<slint::Image> {
    TWEMOJI_TICK.with(|t| t.set(t.get() + 1));
    let now = TWEMOJI_TICK.with(|t| t.get());
    TWEMOJI_CACHE.with(|cache| {
        {
            let mut cache = cache.borrow_mut();
            if let Some((image, last_use)) = cache.get_mut(grapheme) {
                *last_use = now; // touch for LRU bookkeeping
                return image.clone();
            }
        }

        // U+FE0E explicitly requests text presentation. U+FE0F requests emoji
        // presentation, but Twemoji stores some legacy symbols (for example
        // ❤️) under a key without VS16, so retry lookup with VS16 removed.
        let normalized;
        let asset = if grapheme.contains('\u{fe0e}') {
            None
        } else {
            normalized = grapheme.replace('\u{fe0f}', "");
            twemoji_assets::png::PngTwemojiAsset::from_emoji(grapheme).or_else(|| {
                (normalized != grapheme)
                    .then(|| twemoji_assets::png::PngTwemojiAsset::from_emoji(&normalized))
                    .flatten()
            })
        };
        let image = asset
            .and_then(|asset| image::load_from_memory(asset.data.0).ok())
            .map(|decoded| {
                let rgba = decoded.into_rgba8();
                let (width, height) = rgba.dimensions();
                let mut pixels = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(width, height);
                pixels.make_mut_bytes().copy_from_slice(rgba.as_raw());
                slint::Image::from_rgba8(pixels)
            });
        let mut cache = cache.borrow_mut();
        if cache.len() >= TWEMOJI_CACHE_CAP {
            // Evict the least-recently-used half, not all: a full clear makes
            // a screen with more distinct emoji than the cap re-decode every
            // glyph on every frame (thrash). The on-screen working set is
            // touched every frame, so LRU keeps it hot while bounding growth.
            let mut items: Vec<(String, u64)> = cache
                .iter()
                .map(|(k, (_, last))| (k.clone(), *last))
                .collect();
            items.sort_unstable_by_key(|(_, last)| *last);
            let evict = items.len().div_ceil(2);
            for (key, _) in items.into_iter().take(evict) {
                cache.remove(&key);
            }
        }
        cache.insert(grapheme.to_string(), (image.clone(), now));
        image
    })
}
