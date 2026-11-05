use super::*;
use image::{DynamicImage, ImageFormat, Rgba};
use serde_json::json;
use std::{
    sync::atomic::{AtomicUsize, Ordering},
    time::{Duration, Instant},
};

fn encoded(format: ImageFormat, width: u32, height: u32, color: [u8; 4]) -> Vec<u8> {
    let image = DynamicImage::ImageRgba8(RgbaImage::from_pixel(width, height, Rgba(color)));
    let image = if format == ImageFormat::Jpeg {
        DynamicImage::ImageRgb8(image.into_rgb8())
    } else {
        image
    };
    let mut bytes = Cursor::new(Vec::new());
    image.write_to(&mut bytes, format).unwrap();
    bytes.into_inner()
}

fn png(image: RgbaImage) -> Vec<u8> {
    let mut bytes = Cursor::new(Vec::new());
    DynamicImage::ImageRgba8(image)
        .write_to(&mut bytes, ImageFormat::Png)
        .unwrap();
    bytes.into_inner()
}

fn poll_until(service: &Service, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        service.poll();
        if done() {
            break;
        }
        assert!(Instant::now() < deadline, "artwork worker did not finish");
        thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn inventory_metadata_resolves_absolute_relative_and_manifest_fallback_paths() {
    let metadata = json!({"id":"sample", "title":"Regional App", "icon":"regional.png",
        "largeIcon":"large.png", "folderPath":format!("{}/sample", ROOTS[1]), "version":"3.1.0"});
    let apps = crate::platform::normalize_apps(&json!([metadata.clone()])).unwrap();
    assert_eq!(apps[0].icon, Some(Source::new("sample", &metadata)));
    assert_eq!(
        apps[0].icon.as_ref().unwrap().version.as_deref(),
        Some("3.1.0")
    );
    let source = Source::new(
        "sample",
        &json!({
            "folderPath": format!("{}/actual-package-directory", ROOTS[1]),
            "largeIcon": "resources/hu/large.png", "icon": "icon.png"
        }),
    );
    let dirs = source.directories();
    assert_eq!(
        dirs[0],
        Path::new(ROOTS[1]).join("actual-package-directory")
    );
    assert_eq!(
        icon_path(&dirs[0], source.large_icon.as_deref().unwrap()),
        Some(dirs[0].join("resources/hu/large.png"))
    );
    let absolute = format!("file://{}/sample/icon.png", ROOTS[2]);
    assert_eq!(
        icon_path(&dirs[0], &absolute),
        Some(Path::new(ROOTS[2]).join("sample/icon.png"))
    );
    for bad in [
        "../../etc/passwd",
        "/etc/passwd",
        "https://example.com/icon.png",
        "icon\0.png",
    ] {
        assert!(icon_path(&dirs[0], bad).is_none());
    }
    let source = Source::new("sample", &json!({"folderPath":"/etc"}));
    assert_eq!(source.directories().len(), ROOTS.len());
    assert_eq!(source.directories()[0], Path::new(ROOTS[0]).join("sample"));
}

#[test]
fn supported_icons_decode_to_bounded_pixels_and_runtime_card_colours() {
    for format in [ImageFormat::Png, ImageFormat::Jpeg, ImageFormat::WebP] {
        let artwork = decode(&encoded(format, 400, 200, [48, 80, 112, 255])).unwrap();
        assert_eq!(artwork.pixels.dimensions(), (256, 128));
        for stop in artwork.backdrop {
            for (actual, expected) in stop.into_iter().zip([48u8, 80, 112]) {
                assert!(actual.abs_diff(expected) <= 2);
            }
        }
    }
    // Nothing visible at all: the neutral slate backdrop.
    let transparent = decode(&encoded(ImageFormat::Png, 32, 32, [255, 0, 0, 0])).unwrap();
    assert!(
        transparent
            .backdrop
            .iter()
            .all(|stop| luminance(*stop) < 0.2)
    );
    assert!(decode(b"not an image").is_none());
    assert!(decode(&encoded(ImageFormat::Png, 2049, 1, [0; 4])).is_none());
}

fn luminance(rgb: [u8; 3]) -> f32 {
    let linear = |c: u8| {
        let c = c as f32 / 255.;
        if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * linear(rgb[0]) + 0.7152 * linear(rgb[1]) + 0.0722 * linear(rgb[2])
}
fn contrast(a: [u8; 3], b: [u8; 3]) -> f32 {
    let (a, b) = (luminance(a), luminance(b));
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}
/// A logo of one colour on a transparent background: a centred block plus text-like bars.
fn logo(rgb: [u8; 3]) -> RgbaImage {
    RgbaImage::from_fn(200, 200, |x, y| {
        let block = (60..140).contains(&x) && (40..110).contains(&y);
        let bars = (30..170).contains(&x) && (130..160).contains(&y) && x % 12 < 7;
        if block || bars {
            Rgba([rgb[0], rgb[1], rgb[2], 255])
        } else {
            Rgba([0; 4])
        }
    })
}

#[test]
fn transparent_logos_get_a_contrasting_gradient_in_their_own_hue() {
    for (rgb, light_backdrop) in [
        ([255, 255, 255], false),
        ([235, 235, 240], false),
        ([20, 20, 24], true),
        ([200, 30, 40], true),
        ([30, 80, 200], true),
        ([250, 210, 40], false),
        ([40, 200, 120], false),
    ] {
        let backdrop = decode(&png(logo(rgb))).unwrap().backdrop;
        for stop in backdrop {
            assert!(
                contrast(stop, rgb) >= 3.,
                "{rgb:?} on {stop:?}: contrast {}",
                contrast(stop, rgb)
            );
            assert_eq!(luminance(stop) > 0.3, light_backdrop, "{rgb:?} on {stop:?}");
        }
        assert_ne!(
            backdrop[0], backdrop[1],
            "{rgb:?}: a gradient, not a flat fill"
        );
        assert!(
            luminance(backdrop[0]) > luminance(backdrop[1]),
            "{rgb:?}: lighter at the top"
        );
        let saturated = rgb.iter().max().unwrap() - rgb.iter().min().unwrap() > 100;
        if saturated {
            // The backdrop leans to the logo's strongest channel.
            let strongest = (0..3).max_by_key(|&i| rgb[i]).unwrap();
            for stop in backdrop {
                let lean = (0..3).max_by_key(|&i| stop[i]).unwrap();
                assert_eq!(lean, strongest, "{rgb:?} on {stop:?}");
            }
        }
    }
}

#[test]
fn transparent_edges_keep_the_logo_colour_when_scaled() {
    // White on fully transparent black, as many PNG exporters write it: scaling must not
    // pull the transparent black into the antialiased edge, and the GPU's linear filter
    // must find the logo's colour in the transparent texels next to it.
    let image = RgbaImage::from_fn(600, 600, |x, y| {
        if (150..450).contains(&x) && (250..350).contains(&y) {
            Rgba([255, 255, 255, 255])
        } else {
            Rgba([0, 0, 0, 0])
        }
    });
    let pixels = decode(&png(image)).unwrap().pixels;
    for pixel in pixels.pixels().filter(|pixel| pixel[3] > 0) {
        assert!(pixel[0].min(pixel[1]).min(pixel[2]) >= 245, "{pixel:?}");
    }
    let beside = pixels.get_pixel(128, 103);
    assert_eq!(beside[3], 0);
    assert!(beside[0] >= 245, "transparent neighbour {beside:?}");
}

#[test]
fn transparent_logos_reaching_the_edges_keep_the_neutral_card() {
    // Like Litefin's 130px icon: a ring logo on a transparent background whose
    // stroke reaches into the sampled edge band.
    let ring = RgbaImage::from_fn(130, 130, |x, y| {
        let distance = (x as f32 - 64.5).hypot(y as f32 - 64.5);
        if (52.0..62.0).contains(&distance) {
            Rgba([150, 100, 210, 255])
        } else {
            Rgba([0; 4])
        }
    });
    let backdrop = decode(&png(ring)).unwrap().backdrop;
    for stop in backdrop {
        assert!(contrast(stop, [150, 100, 210]) >= 3., "{stop:?}");
    }
    // Opaque artwork with transparent rounded corners still takes its edge colour.
    let rounded = RgbaImage::from_fn(130, 130, |x, y| {
        let inset = |v: u32| v.min(129 - v) as f32;
        let (dx, dy) = (24. - inset(x), 24. - inset(y));
        if dx > 0. && dy > 0. && dx.hypot(dy) > 24. {
            Rgba([0; 4])
        } else {
            Rgba([40, 90, 160, 255])
        }
    });
    for stop in decode(&png(rounded)).unwrap().backdrop {
        for (actual, expected) in stop.into_iter().zip([40u8, 90, 160]) {
            assert!(actual.abs_diff(expected) <= 2);
        }
    }
}

#[test]
fn local_reads_fall_back_from_a_corrupt_large_icon_to_manifest_artwork() {
    let directory = Path::new(ROOTS[1]).join("sample");
    let manifest = json!({"id":"sample", "largeIcon":"corrupt.png", "icon":"local/icon.webp"});
    let files = HashMap::from([
        (
            directory.join("appinfo.json"),
            serde_json::to_vec(&manifest).unwrap(),
        ),
        (directory.join("corrupt.png"), b"broken".to_vec()),
        (
            directory.join("local/icon.webp"),
            encoded(ImageFormat::WebP, 32, 32, [8, 16, 24, 255]),
        ),
    ]);
    let source = Source::new("sample", &json!({"folderPath": directory}));
    let mut reads = Vec::new();
    let image = load_local(&source, |path, limit| {
        reads.push(path.to_owned());
        files
            .get(path)
            .filter(|bytes| bytes.len() <= limit)
            .cloned()
    })
    .unwrap();
    assert_eq!(image.pixels.get_pixel(0, 0).0, [8, 16, 24, 255]);
    assert_eq!(reads[0], directory.join("appinfo.json"));
    assert!(reads.contains(&directory.join("corrupt.png")));
}

#[test]
fn refresh_discards_old_worker_results_and_reloads_changed_artwork() {
    let calls = Arc::new(AtomicUsize::new(0));
    let worker_calls = calls.clone();
    let (started, received) = mpsc::channel();
    let ui_thread = thread::current().id();
    let service = Service::with_loader(true, move |_| {
        assert_ne!(thread::current().id(), ui_thread);
        let number = worker_calls.fetch_add(1, Ordering::SeqCst) + 1;
        started.send(number).unwrap();
        decode(&encoded(ImageFormat::Png, 2, 2, [number as u8, 0, 0, 255]))
    });
    let source = Source::new("sample", &json!({}));
    assert!(service.get(&source).is_none());
    assert_eq!(received.recv_timeout(Duration::from_secs(3)).unwrap(), 1);
    service.reset();
    assert!(service.get(&source).is_none());
    poll_until(&service, || service.get(&source).is_some());
    let image = service.get(&source).unwrap();
    assert_eq!(image.pixels.get_pixel(0, 0)[0], 2);
    assert!(Arc::ptr_eq(&image, &service.get(&source).unwrap()));
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[test]
fn the_worker_keeps_decoding_while_the_ui_is_idle() {
    let calls = Arc::new(AtomicUsize::new(0));
    let worker_calls = calls.clone();
    let service = Service::with_loader(true, move |_| {
        worker_calls.fetch_add(1, Ordering::SeqCst);
        decode(&encoded(ImageFormat::Png, 2, 2, [1, 2, 3, 255]))
    });
    let sources: Vec<_> = (0..5)
        .map(|number| Source::new(&format!("app-{number}"), &json!({})))
        .collect();
    for source in &sources {
        assert!(service.get(source).is_none());
    }
    assert!(service.pending());
    // No poll yet: an idle UI must not hold the worker to one finished icon.
    let deadline = Instant::now() + Duration::from_secs(3);
    while calls.load(Ordering::SeqCst) < sources.len() {
        assert!(
            Instant::now() < deadline,
            "worker stalled until the UI polled"
        );
        thread::sleep(Duration::from_millis(1));
    }
    poll_until(&service, || {
        sources.iter().all(|source| service.get(source).is_some())
    });
    assert!(!service.pending());
}

#[test]
fn a_refresh_keeps_unchanged_icons_and_reads_updated_apps_again() {
    let calls = Arc::new(AtomicUsize::new(0));
    let worker_calls = calls.clone();
    let ui_thread = thread::current().id();
    let service = Service::with_loader(true, move |_| {
        assert_ne!(thread::current().id(), ui_thread);
        let number = worker_calls.fetch_add(1, Ordering::SeqCst) + 1;
        decode(&encoded(ImageFormat::Png, 2, 2, [number as u8, 0, 0, 255]))
    });
    let installed = Source::new("sample", &json!({"version": "1.0.0"}));
    assert!(service.get(&installed).is_none());
    poll_until(&service, || service.get(&installed).is_some());
    let image = service.get(&installed).unwrap();
    service.refresh();
    // Unchanged after the refresh: the same decoded artwork, not read again.
    assert!(Arc::ptr_eq(&image, &service.get(&installed).unwrap()));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    // An update changes the version, so its icon is read again.
    let updated = Source::new("sample", &json!({"version": "1.1.0"}));
    assert_ne!(installed, updated);
    assert!(service.get(&updated).is_none());
    poll_until(&service, || service.get(&updated).is_some());
    assert_eq!(service.get(&updated).unwrap().pixels.get_pixel(0, 0)[0], 2);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[test]
fn a_refresh_during_a_load_keeps_its_result() {
    let calls = Arc::new(AtomicUsize::new(0));
    let worker_calls = calls.clone();
    let (release, hold) = mpsc::channel::<()>();
    let service = Service::with_loader(true, move |_| {
        worker_calls.fetch_add(1, Ordering::SeqCst);
        hold.recv().unwrap();
        decode(&encoded(ImageFormat::Png, 2, 2, [1, 0, 0, 255]))
    });
    let source = Source::new("sample", &json!({}));
    assert!(service.get(&source).is_none());
    service.refresh();
    release.send(()).unwrap();
    poll_until(&service, || service.get(&source).is_some());
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[test]
fn unavailable_icons_are_not_retried_on_every_redraw() {
    let calls = Arc::new(AtomicUsize::new(0));
    let worker_calls = calls.clone();
    let service = Service::with_loader(true, move |_| {
        worker_calls.fetch_add(1, Ordering::SeqCst);
        None
    });
    let source = Source::new("sample", &json!({}));
    assert!(service.get(&source).is_none());
    poll_until(&service, || {
        service.cache.borrow().entries[&source].image.is_some()
    });
    for _ in 0..10 {
        assert!(service.get(&source).is_none());
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    // Unavailable icons are tried again after a refresh.
    service.refresh();
    assert!(service.get(&source).is_none());
    poll_until(&service, || {
        service.cache.borrow().entries[&source].image.is_some()
    });
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[test]
fn hundreds_of_apps_stay_within_the_cache_bounds_across_refreshes() {
    let service = Service::with_loader(true, |_| {
        Some(Artwork {
            key: String::new(),
            pixels: RgbaImage::new(256, 256),
            backdrop: [[20; 3]; 2],
        })
    });
    for number in 0..300 {
        if number % 100 == 0 {
            service.refresh();
        }
        let source = Source::new(&format!("app-{number}"), &json!({"version": "1"}));
        service.get(&source);
        poll_until(&service, || {
            service.cache.borrow().entries[&source].image.is_some()
        });
    }
    let cache = service.cache.borrow();
    let bytes: usize = cache
        .entries
        .values()
        .filter_map(|entry| entry.image.as_ref()?.as_ref())
        .map(|image| image.pixels.as_raw().len())
        .sum();
    assert!(bytes <= CACHE_BYTES);
    assert!(cache.entries.len() <= CACHE_ENTRIES);
}

#[test]
fn decoded_cache_evicts_old_icons_when_scrolling_through_many_apps() {
    let service = Service::with_loader(true, |_| {
        Some(Artwork {
            key: String::new(),
            pixels: RgbaImage::new(256, 256),
            backdrop: [[20; 3]; 2],
        })
    });
    for number in 0..80 {
        let source = Source::new(&format!("app-{number}"), &json!({}));
        service.get(&source);
        poll_until(&service, || {
            service.cache.borrow().entries[&source].image.is_some()
        });
    }
    let cache = service.cache.borrow();
    let bytes: usize = cache
        .entries
        .values()
        .filter_map(|entry| entry.image.as_ref()?.as_ref())
        .map(|image| image.pixels.as_raw().len())
        .sum();
    assert!(bytes <= CACHE_BYTES);
    assert!(
        !cache
            .entries
            .contains_key(&Source::new("app-0", &json!({})))
    );
    assert!(
        cache
            .entries
            .contains_key(&Source::new("app-79", &json!({})))
    );
}

#[test]
fn favorite_badge_uses_the_local_gradient_and_maximum_black_white_contrast() {
    let mut artwork = decode(&encoded(ImageFormat::Png, 32, 32, [255; 4])).unwrap();
    assert!(artwork.dark_badge(0.));
    artwork.backdrop = [[0; 3]; 2];
    assert!(!artwork.dark_badge(0.));
    artwork.backdrop = [[255; 3], [0; 3]];
    assert!(artwork.dark_badge(19.2 / 154.4));
    assert!(!artwork.dark_badge(1.));
    // Choose by linear luminance, not an arbitrary sRGB brightness threshold.
    for value in 0..=255 {
        artwork.backdrop = [[value; 3]; 2];
        let background = luminance([value; 3]);
        let foreground = if artwork.dark_badge(0.125) { 0. } else { 1. };
        assert!(
            super::contrast(foreground, background) >= super::contrast(1. - foreground, background)
        );
        assert!(super::contrast(foreground, background) >= 4.5);
    }
}

/// The glass plate is dark smoked glass whatever the installed icon looks like, so the badge is
/// always the same white mark with a dark offset under it, rather than a colour picked from the
/// icon's own backdrop.
#[test]
fn both_grids_and_the_options_card_use_one_white_heart_with_a_dark_offset() {
    use crate::renderer::{Color, Draw};
    use crate::state::Target;
    const SHADOW: Color = [3. / 255., 6. / 255., 10. / 255., 0.38];
    const MARK: Color = [1.; 4];
    // A plain white icon and a plain black one: the plate is what decides the badge colour, so
    // neither may move it.
    for pixel in [[255; 4], [0, 0, 0, 255]] {
        let assets = crate::view::Assets {
            root: PathBuf::new(),
            version: "test".into(),
            metrics: Default::default(),
            artwork: Service::with_loader(true, move |_| {
                decode(&encoded(ImageFormat::Png, 32, 32, pixel))
            }),
        };
        let apps = crate::platform::normalize_apps(&json!([
            {"id":"regional.app", "title":"Regional App"}
        ]))
        .unwrap();
        let mut state = crate::state::State::new(crate::model::Preferences {
            favorites: Some(vec!["regional.app".into()]),
            reduced_motion: true,
            ..Default::default()
        });
        state.set_inventory(
            crate::model::Inventory {
                apps,
                inputs: vec![],
            },
            None,
        );
        state.open(1);
        state.focus = Target::App("regional.app".into());
        let render = |state: &crate::state::State| {
            crate::view::scene_with_pill(
                state,
                &assets,
                800.,
                100.,
                time::OffsetDateTime::UNIX_EPOCH,
            )
        };
        poll_until(&assets.artwork, || {
            render(&state)
                .draw
                .iter()
                .any(|draw| matches!(draw, Draw::AppIcon(..)))
        });
        for popup in [false, true] {
            if popup {
                state.options();
            }
            let scene = render(&state);
            let hearts: Vec<_> = scene
                .draw
                .iter()
                .filter_map(|draw| match draw {
                    Draw::TintedImage(path, rect, _, _, color)
                        if path.ends_with("heart-filled.lvec") && rect.w == 16. =>
                    {
                        Some(*color)
                    }
                    _ => None,
                })
                .collect();
            // One badge on the tile that stands in the grid, and a second on the card the popup
            // lifts out of it: a dark offset painted first, then the white mark over it.
            let badges = if popup { 2 } else { 1 };
            let mut expected: Vec<Color> = Vec::new();
            for _ in 0..badges {
                expected.extend([SHADOW, MARK]);
            }
            assert_eq!(
                hearts, expected,
                "one white mark over a dark offset for every badge"
            );
            assert!(!scene.draw.iter().any(|draw| matches!(draw,
                Draw::Shape(rect, radius, _, _) if rect.w == 28. && rect.h == 28. && *radius == 14.
            )));
        }
    }
}

#[test]
fn visible_cards_switch_from_fallback_to_an_inset_icon_on_a_glass_plate() {
    let assets = crate::view::Assets {
        root: PathBuf::new(),
        version: "test".into(),
        metrics: Default::default(),
        artwork: Service::with_loader(true, |_| {
            decode(&encoded(ImageFormat::Png, 32, 32, [24, 40, 56, 255]))
        }),
    };
    let apps =
        crate::platform::normalize_apps(&json!([{"id":"regional.app", "title":"Regional App"}]))
            .unwrap();
    let mut state = crate::state::State::new(Default::default());
    state.set_inventory(
        crate::model::Inventory {
            apps,
            inputs: vec![],
        },
        None,
    );
    let scene = || {
        crate::view::scene_with_pill(
            &state,
            &assets,
            800.,
            100.,
            time::OffsetDateTime::UNIX_EPOCH,
        )
    };
    let icons = |scene: &crate::view::Scene| {
        scene
            .draw
            .iter()
            .filter(|draw| matches!(draw, crate::renderer::Draw::AppIcon(..)))
            .count()
    };
    assert_eq!(
        icons(&scene()),
        0,
        "the generic surface stands in until the icon decodes"
    );
    poll_until(&assets.artwork, || icons(&scene()) > 0);
    let loaded = scene();
    assert_eq!(icons(&loaded), 1, "one installed icon per visible card");
    assert!(
        !loaded
            .draw
            .iter()
            .any(|draw| matches!(draw, crate::renderer::Draw::AppImage(..))),
        "nothing composites the backdrop into the icon any more"
    );
    let (face, tint) = loaded
        .draw
        .iter()
        .find_map(|draw| match draw {
            crate::renderer::Draw::AppIcon(_, rect, tint) => Some((*rect, *tint)),
            _ => None,
        })
        .unwrap();
    assert_eq!(tint, [1.; 4], "the icon keeps its own alpha over the plate");
    // The plate it sits on: a dark translucent gradient over the whole face, so the glass shows
    // around the inset icon and a white badge stays legible on top of it. The tile's own rim
    // reflection has the same shape, so the dark stops are what pick out the glass.
    let glass_on_face = |draw: &crate::renderer::Draw| match draw {
        crate::renderer::Draw::RoundedGradient(rect, radius, top, bottom) => {
            let dark = |stop: &[f32; 4]| (stop[0] + stop[1] + stop[2]) / 3. < 0.3;
            let covers = rect.w < 400.
                && rect.x < face.x
                && rect.y < face.y
                && rect.x + rect.w > face.x + face.w
                && rect.y + rect.h > face.y + face.h;
            (covers && dark(top) && dark(bottom)).then(|| (*rect, *radius, *top, *bottom))
        }
        _ => None,
    };
    let plate = loaded.draw.iter().find_map(glass_on_face).unwrap();
    assert!(plate.1 > 0., "the plate is a rounded face");
    for stop in [plate.2, plate.3] {
        assert!(stop[3] < 1., "{stop:?} is opaque, not smoked glass");
    }
    // The rim runs along that same face, and the icon sits centred inside it.
    let same_face = |r: &crate::state::Rect| {
        r.x == plate.0.x && r.y == plate.0.y && r.w == plate.0.w && r.h == plate.0.h
    };
    assert!(loaded.draw.iter().any(|draw| matches!(draw,
        crate::renderer::Draw::Shape(rect, radius, border, _)
            if same_face(rect) && *radius == plate.1 && *border == 1.
    )));
    let (centre_x, centre_y) = face.center();
    let (plate_x, plate_y) = plate.0.center();
    assert!(
        (centre_x - plate_x).abs() < 0.01 && (centre_y - plate_y).abs() < 0.01,
        "the icon is centred on the plate"
    );
    assert!(
        face.w < plate.0.w && face.h < plate.0.h,
        "the glass shows around the icon"
    );
}

#[test]
fn prefetch_decodes_every_icon_although_the_request_queue_is_short() {
    let (release, gate) = mpsc::channel::<()>();
    let gate = std::sync::Mutex::new(gate);
    let service = Service::with_loader(true, move |_| {
        gate.lock().unwrap().recv().unwrap();
        None
    });
    let sources: Vec<_> = (0..100)
        .map(|index| Source::new(&format!("sample{index}"), &json!({})))
        .collect();
    service.prefetch(sources.clone());
    // Slots stay free for the cards on screen while the worker is busy.
    assert!(service.cache.borrow().entries.len() <= PREFETCH_SLOTS);
    assert!(service.get(&Source::new("visible", &json!({}))).is_none());
    for _ in 0..101 {
        release.send(()).unwrap();
    }
    poll_until(&service, || !service.pending());
    let cache = service.cache.borrow();
    assert!(
        sources
            .iter()
            .all(|source| cache.entries.contains_key(source))
    );
    assert!(
        cache
            .entries
            .contains_key(&Source::new("visible", &json!({})))
    );
}

#[test]
fn visible_requests_and_promoted_prefetches_run_before_background_backlog() {
    let (started, observed) = mpsc::channel();
    let (release, gate) = mpsc::channel();
    let service = Service::with_loader(true, move |source| {
        started.send(source.id.clone()).unwrap();
        gate.recv().unwrap();
        None
    });
    let sources: Vec<_> = (0..100)
        .map(|i| Source::new(&format!("background-{i}"), &json!({})))
        .collect();
    service.prefetch(sources.clone());
    assert_eq!(
        observed.recv_timeout(Duration::from_secs(3)).unwrap(),
        "background-0"
    );
    service.get(&Source::new("visible", &json!({})));
    service.get(&sources[7]);
    release.send(()).unwrap();
    assert_eq!(
        observed.recv_timeout(Duration::from_secs(3)).unwrap(),
        "visible"
    );
    release.send(()).unwrap();
    assert_eq!(
        observed.recv_timeout(Duration::from_secs(3)).unwrap(),
        "background-7"
    );
    for _ in 0..99 {
        release.send(()).unwrap();
    }
    poll_until(&service, || !service.pending());
    assert!(
        sources
            .iter()
            .all(|source| service.cache.borrow().entries.contains_key(source))
    );
    // Promotion reuses the pending request rather than decoding the icon twice.
    assert_eq!(observed.try_iter().count(), 98);
}

#[test]
fn reset_discards_prefetch_backlog_and_queued_requests() {
    let (started, observed) = mpsc::channel();
    let (release, gate) = mpsc::channel();
    let service = Service::with_loader(true, move |source| {
        started.send(source.id.clone()).unwrap();
        gate.recv().unwrap();
        None
    });
    service.prefetch((0..100).map(|i| Source::new(&format!("old-{i}"), &json!({}))));
    assert_eq!(
        observed.recv_timeout(Duration::from_secs(3)).unwrap(),
        "old-0"
    );
    service.reset();
    let current = Source::new("current", &json!({}));
    service.prefetch([current.clone()]);
    release.send(()).unwrap();
    assert_eq!(
        observed.recv_timeout(Duration::from_secs(3)).unwrap(),
        "current"
    );
    release.send(()).unwrap();
    poll_until(&service, || !service.pending());
    assert_eq!(service.cache.borrow().entries.len(), 1);
    assert!(service.cache.borrow().entries.contains_key(&current));
    assert!(observed.try_recv().is_err());
}
