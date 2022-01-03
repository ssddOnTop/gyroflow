pub mod gyro_source;
pub mod integration;
pub mod integration_complementary; // TODO: add this to `ahrs` crate
pub mod lens_profile;
pub mod lens_profile_database;
pub mod calibration;
pub mod synchronization;
pub mod undistortion;
pub mod adaptive_zoom;
pub mod camera_identifier;

pub mod smoothing;
pub mod filtering;

pub mod gpu;

use std::sync::{ Arc, atomic::Ordering::Relaxed };
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering::SeqCst;
use camera_identifier::CameraIdentifier;
use parking_lot::RwLock;
pub use undistortion::PixelType;

use crate::lens_profile_database::LensProfileDatabase;

use self::{ lens_profile::LensProfile, smoothing::Smoothing, undistortion::Undistortion, adaptive_zoom::AdaptiveZoom, calibration::LensCalibrator };

use simd_json::ValueAccess;
use nalgebra::{ Quaternion, Vector3, Vector4 };
use gyro_source::{ GyroSource, Quat64, TimeIMU };
use telemetry_parser::try_block;

lazy_static::lazy_static! {
    static ref THREAD_POOL: rayon::ThreadPool = rayon::ThreadPoolBuilder::new().build().unwrap();
}

#[derive(Clone)]
pub struct BasicParams {
    pub size: (usize, usize), // Processing input size
    pub output_size: (usize, usize), // Processing output size
    pub video_size: (usize, usize), // Full resolution input size
    pub video_output_size: (usize, usize), // Full resoution output size

    pub background: Vector4<f32>,

    pub frame_readout_time: f64,
    pub adaptive_zoom_window: f64,
    pub fov: f64,
    pub fovs: Vec<f64>,
    pub fps: f64,
    pub fps_scale: Option<f64>,
    pub frame_count: usize,
    pub duration_ms: f64,

    pub trim_start: f64,
    pub trim_end: f64,

    pub video_rotation: f64,

    pub framebuffer_inverted: bool,
    pub is_calibrator: bool,
    
    pub stab_enabled: bool,
    pub show_detected_features: bool,
    pub show_optical_flow: bool,
}
impl Default for BasicParams {
    fn default() -> Self {
        Self {
            fov: 1.0,
            fovs: vec![],
            stab_enabled: true,
            show_detected_features: true,
            show_optical_flow: true,
            frame_readout_time: 0.0, 
            adaptive_zoom_window: 0.0, 

            size: (0, 0),
            output_size: (0, 0),
            video_size: (0, 0),
            video_output_size: (0, 0),

            video_rotation: 0.0,
            
            framebuffer_inverted: false,
            is_calibrator: false,

            trim_start: 0.0,
            trim_end: 1.0,
        
            background: Vector4::new(0.0, 0.0, 0.0, 0.0),
    
            fps: 0.0,
            fps_scale: None,
            frame_count: 0,
            duration_ms: 0.0,
        }
    }
}

impl BasicParams {
    pub fn get_scaled_duration_ms(&self) -> f64 {
        match self.fps_scale {
            Some(scale) => self.duration_ms / scale,
            None            => self.duration_ms
        }
    }
    pub fn get_scaled_fps(&self) -> f64 {
        match self.fps_scale {
            Some(scale) => self.fps / scale,
            None            => self.fps
        }
    }
}

pub struct StabilizationManager<T: PixelType> {
    pub gyro: Arc<RwLock<GyroSource>>,
    pub lens: Arc<RwLock<LensProfile>>,
    pub smoothing: Arc<RwLock<Smoothing>>,

    pub undistortion: Arc<RwLock<Undistortion<T>>>,

    pub pose_estimator: Arc<synchronization::PoseEstimator>,
    pub lens_calibrator: Arc<RwLock<Option<LensCalibrator>>>,

    pub current_compute_id: Arc<AtomicU64>,
    pub smoothness_checksum: Arc<AtomicU64>,
    pub adaptive_zoom_checksum: Arc<AtomicU64>,

    pub camera_id: Arc<RwLock<Option<CameraIdentifier>>>,
    pub lens_profile_db: Arc<RwLock<LensProfileDatabase>>,

    pub params: Arc<RwLock<BasicParams>>
}

impl<T: PixelType> Default for StabilizationManager<T> {
    fn default() -> Self {
        Self {
            smoothing: Arc::new(RwLock::new(Smoothing::default())),

            params: Arc::new(RwLock::new(BasicParams::default())),
            
            undistortion: Arc::new(RwLock::new(Undistortion::<T>::default())),
            gyro: Arc::new(RwLock::new(GyroSource::new())),
            lens: Arc::new(RwLock::new(LensProfile::default())),
            
            current_compute_id: Arc::new(AtomicU64::new(0)),
            smoothness_checksum: Arc::new(AtomicU64::new(0)),
            adaptive_zoom_checksum: Arc::new(AtomicU64::new(0)),
            
            pose_estimator: Arc::new(synchronization::PoseEstimator::default()),

            lens_profile_db: Arc::new(RwLock::new(LensProfileDatabase::default())),

            lens_calibrator: Arc::new(RwLock::new(None)),

            camera_id: Arc::new(RwLock::new(None)),
        }
    }
}

impl<T: PixelType> StabilizationManager<T> {
    pub fn init_from_video_data(&self, path: &str, duration_ms: f64, fps: f64, frame_count: usize, video_size: (usize, usize)) -> std::io::Result<()> {
        {
            let mut params = self.params.write();
            params.fps = fps;
            params.frame_count = frame_count;
            params.duration_ms = duration_ms;
            params.video_size = video_size;
        }

        self.pose_estimator.sync_results.write().clear();

        let _ = self.load_gyro_data(path); // Ignore the error here, video file may not contain the telemetry and it's ok
        
        Ok(())
    }

    pub fn load_gyro_data(&self, path: &str) -> std::io::Result<()> {
        {
            let params = self.params.read();
            let mut gyro = self.gyro.write();
            gyro.init_from_params(&params);
        }

        if path.ends_with(".gyroflow") {
            let mut data = std::fs::read(path)?;
            let v = simd_json::to_borrowed_value(&mut data)?;
    
            self.lens.write().load_from_json_value(&v["calibration_data"]);

            let to_f64_array = |x: &simd_json::borrowed::Value| -> Option<Vec<f64>> { Some(x.as_array()?.iter().filter_map(|x| x.as_f64()).collect()) };

            try_block!({
                //let smoothed_quaternions = v["stab_transform"].as_array()?.iter().filter_map(to_f64_array)
                //    .map(|x| ((x[0] * 1000.0) as i64, Quat64::from_quaternion(Quaternion::from_parts(x[3], Vector3::new(x[4], x[5], x[6])))))
                //    .collect();
        
                let quaternions = v["frame_orientation"].as_array()?.iter().filter_map(to_f64_array)
                    .map(|x| ((x[1] * 1000000.0) as i64, Quat64::from_quaternion(Quaternion::from_parts(x[3-1], Vector3::new(x[4-1], x[5-1], x[6-1])))))
                    .collect();
        
                let raw_imu = v["raw_imu"].as_array()?.iter().filter_map(to_f64_array)
                    .map(|x| TimeIMU { timestamp_ms: 0.0/*TODO*/, gyro: Some([x[0], x[1], x[2]]), accl: Some([x[3], x[4], x[6]]), magn: None })
                    .collect();

                let md = crate::gyro_source::FileMetadata {
                    imu_orientation: None, // TODO IMU orientation
                    detected_source: Some("Gyroflow file".to_string()),
                    quaternions: Some(quaternions),
                    raw_imu: Some(raw_imu),
                    frame_readout_time: None,
                    camera_identifier: None,
                };
                self.gyro.write().load_from_telemetry(&md);
                self.smoothing.write().update_quats_checksum(&self.gyro.read().quaternions);
            });
        } else {
            let (fps, size) = {
                let params = self.params.read();
                (params.fps, params.video_size)
            };

            let md = GyroSource::parse_telemetry_file(path, size, fps)?;
            self.gyro.write().load_from_telemetry(&md);
            self.params.write().frame_readout_time = md.frame_readout_time.unwrap_or_default();
            self.smoothing.write().update_quats_checksum(&self.gyro.read().quaternions);
            if let Some(id) = md.camera_identifier {
                *self.camera_id.write() = Some(id);
            }
        }
        Ok(())
    }

    pub fn load_lens_profile(&self, path: &str) {
        self.lens.write().load_from_file(path); // TODO Result
    }

    fn init_size(&self) {
        let (w, h, ow, oh, bg) = {
            let params = self.params.read();
            (params.size.0, params.size.1, params.output_size.0, params.output_size.1, params.background)
        };

        let s = w * T::COUNT * T::SCALAR_BYTES;
        let os = ow * T::COUNT * T::SCALAR_BYTES;

        if w > 0 && ow > 0 && h > 0 && oh > 0 {
            self.undistortion.write().init_size(bg, (w, h), s, (ow, oh), os);
        }
    }

    pub fn set_size(&self, width: usize, height: usize) {
        {
            let mut params = self.params.write();
            params.size = (width, height);

            let ratio = params.size.0 as f64 / params.video_size.0 as f64;
            params.output_size = ((params.video_output_size.0 as f64 * ratio) as usize, (params.video_output_size.1 as f64 * ratio) as usize);
        }
        self.init_size();
    }
    pub fn set_output_size(&self, width: usize, height: usize) {
        {
            let mut params = self.params.write();
            let ratio = params.size.0 as f64 / params.video_size.0 as f64;
            params.output_size = ((width as f64 * ratio) as usize, (height as f64 * ratio) as usize);
            params.video_output_size = (width, height);
        }
        self.init_size();
    }

    pub fn recompute_adaptive_zoom_static(zoom: &mut AdaptiveZoom, params: &RwLock<BasicParams>, gyro: &RwLock<GyroSource>) -> Vec<f64> {
        let (window, frames, fps) = {
            let params = params.read();
            (params.adaptive_zoom_window, params.frame_count, params.get_scaled_fps())
        };
        if window > 0.0 || window < -0.9 {
            let mut quats = Vec::with_capacity(frames);
            {
                let g = gyro.read();
                for i in 0..frames {
                    quats.push(g.smoothed_quat_at_timestamp(i as f64 * 1000.0 / fps));
                }
            }

            let fovs = zoom.compute(&quats);
            fovs.iter().map(|v| v.0).collect()
        } else {
            Vec::new()
        }
    }
    pub fn recompute_adaptive_zoom(&self) {
        let mut zoom = AdaptiveZoom::from_manager(self);
        let fovs = Self::recompute_adaptive_zoom_static(&mut zoom, &self.params, &self.gyro);
        self.params.write().fovs = fovs;
    }

    pub fn recompute_smoothness(&self) {
        self.gyro.write().recompute_smoothness(self.smoothing.write().current().as_ref());
    }

    pub fn recompute_undistortion(&self) {
        let params = undistortion::ComputeParams::from_manager(self);
        self.undistortion.write().set_compute_params(params);
    }

    pub fn recompute_blocking(&self) {
        self.recompute_smoothness();
        self.recompute_adaptive_zoom();
        self.recompute_undistortion();
    }
    
    pub fn recompute_threaded<F: Fn(u64) + Send + Sync + Clone + 'static>(&self, cb: F) -> u64 {
        //self.recompute_smoothness();
        //self.recompute_adaptive_zoom();
        let mut params = undistortion::ComputeParams::from_manager(self);

        let smoothing = self.smoothing.clone();
        let basic_params = self.params.clone();
        let gyro = self.gyro.clone();
        let mut zoom = AdaptiveZoom::from_manager(self);

        let compute_id = fastrand::u64(..);
        self.current_compute_id.store(compute_id, SeqCst);

        let current_compute_id = self.current_compute_id.clone();
        let smoothness_checksum = self.smoothness_checksum.clone();
        let adaptive_zoom_checksum = self.adaptive_zoom_checksum.clone();

        let undistortion = self.undistortion.clone();
        THREAD_POOL.spawn(move || {
            // std::thread::sleep(std::time::Duration::from_millis(20));
            if current_compute_id.load(Relaxed) != compute_id { return; }

            let mut smoothing_changed = false;
            if smoothing.read().get_state_checksum() != smoothness_checksum.load(SeqCst) {
                let smoothing = smoothing.write().current().clone();
                params.gyro.recompute_smoothness(smoothing.as_ref());

                let mut lib_gyro = gyro.write();
                lib_gyro.quaternions = params.gyro.quaternions.clone();
                lib_gyro.smoothed_quaternions = params.gyro.smoothed_quaternions.clone();
                lib_gyro.org_smoothed_quaternions = params.gyro.org_smoothed_quaternions.clone();
                smoothing_changed = true;
            }
            
            if current_compute_id.load(Relaxed) != compute_id { return; }

            if smoothing_changed || zoom.get_state_checksum() != adaptive_zoom_checksum.load(SeqCst) {
                let lock = RwLock::new(params.gyro.clone());
                params.fovs = Self::recompute_adaptive_zoom_static(&mut zoom, &basic_params, &lock);
                basic_params.write().fovs = params.fovs.clone();
            }
            
            if current_compute_id.load(Relaxed) != compute_id { return; }

            undistortion.write().set_compute_params(params);

            smoothness_checksum.store(smoothing.read().get_state_checksum(), SeqCst);
            adaptive_zoom_checksum.store(zoom.get_state_checksum(), SeqCst);
            cb(compute_id);
        });
        compute_id
    }

    pub fn get_features_pixels(&self, frame: usize) -> Option<Vec<(i32, i32, f32)>> { // (x, y, alpha)
        let mut ret = None;
        if self.params.read().show_detected_features {
            let (xs, ys) = self.pose_estimator.get_points_for_frame(&frame);
            for i in 0..xs.len() {
                if ret.is_none() {
                    // Only allocate if we actually have any points
                    ret = Some(Vec::with_capacity(2048));
                }
                for xstep in -1..=1i32 {
                    for ystep in -1..=1i32 {
                        ret.as_mut().unwrap().push((xs[i] as i32 + xstep, ys[i] as i32 + ystep, 1.0));
                    }
                }
            }
        }
        ret
    }
    pub fn get_opticalflow_pixels(&self, frame: usize) -> Option<Vec<(i32, i32, f32)>> { // (x, y, alpha)
        let mut ret = None;
        if self.params.read().show_optical_flow {
            for i in 0..3 {
                let a = (3 - i) as f32 / 3.0;
                if let Some(lines) = self.pose_estimator.get_of_lines_for_frame(&(frame + i), 1.0, 1) {
                    lines.0.into_iter().zip(lines.1.into_iter()).for_each(|(p1, p2)| {
                        if ret.is_none() {
                            // Only allocate if we actually have any points
                            ret = Some(Vec::with_capacity(2048));
                        }
                        let line = line_drawing::Bresenham::new((p1.0 as isize, p1.1 as isize), (p2.0 as isize, p2.1 as isize)); 
                        for point in line {
                            ret.as_mut().unwrap().push((point.0 as i32, point.1 as i32, a));
                        }
                    });
                }
            }
        }
        ret
    }

    pub unsafe fn fill_undistortion_data_padded(&self, timestamp_us: i64, out_ptr: *mut f32, out_size: usize) -> bool {
        if self.params.read().stab_enabled {
            if let Some(itm) = self.undistortion.write().get_undistortion_data(timestamp_us) {
                let params_count = itm.params.len() * 9;
                if params_count <= out_size {
                    let src_ptr = itm.params.as_ptr() as *const f32;
                    std::ptr::copy_nonoverlapping(src_ptr, out_ptr, 8);

                    let mut j = 2;
                    for i in (9..params_count).step_by(3) {
                        std::ptr::copy_nonoverlapping(src_ptr.add(i), out_ptr.offset(j * 4), 3);
                        j += 1;
                    }
                    return true;
                }
            }
        }
        false
    }

    pub fn process_pixels(&self, mut timestamp_us: i64, width: usize, height: usize, stride: usize, out_width: usize, out_height: usize, out_stride: usize, pixels: &mut [u8], out_pixels: &mut [u8]) -> bool {
        let (enabled, ow, oh, framebuffer_inverted, fps, fps_scale, is_calibrator) = {
            let params = self.params.read();
            (params.stab_enabled, params.output_size.0, params.output_size.1, params.framebuffer_inverted, params.fps, params.fps_scale, params.is_calibrator)
        };
        if enabled && ow == out_width && oh == out_height {
            if let Some(scale) = fps_scale {
                timestamp_us = (timestamp_us as f64 / scale).round() as i64;
            }
            let frame = frame_at_timestamp(timestamp_us as f64 / 1000.0, fps) as usize; // used only to draw features and OF
            //////////////////////////// Draw detected features ////////////////////////////
            // TODO: maybe handle other types than RGBA8?
            if T::COUNT == 4 && T::SCALAR_BYTES == 1 {
                if let Some(pxs) = self.get_features_pixels(frame) {
                    for (x, mut y, _) in pxs {
                        if framebuffer_inverted { y = height as i32 - y; }
                        let pos = (y * stride as i32 + x * (T::COUNT * T::SCALAR_BYTES) as i32) as usize;
                        if pixels.len() > pos + 2 { 
                            pixels[pos + 0] = 0x0c; // R
                            pixels[pos + 1] = 0xff; // G
                            pixels[pos + 2] = 0x00; // B
                        }
                    }
                }
                if let Some(pxs) = self.get_opticalflow_pixels(frame) {
                    for (x, mut y, a) in pxs {
                        if framebuffer_inverted { y = height as i32 - y; }
                        let pos = (y * stride as i32 + x * (T::COUNT * T::SCALAR_BYTES) as i32) as usize;
                        if pixels.len() > pos + 2 { 
                            pixels[pos + 0] = (pixels[pos + 0] as f32 * (1.0 - a) + 0xfe as f32 * a) as u8; // R
                            pixels[pos + 1] = (pixels[pos + 1] as f32 * (1.0 - a) + 0xfb as f32 * a) as u8; // G
                            pixels[pos + 2] = (pixels[pos + 2] as f32 * (1.0 - a) + 0x47 as f32 * a) as u8; // B
                        }
                    }
                }
                if is_calibrator {
                    let lock = self.lens_calibrator.read();
                    if let Some(ref cal) = *lock {
                        let points = cal.all_matches.read();
                        match points.get(&(frame as i32)) {
                            Some(entry) => {
                                let (w, h, s) = (width as u32, height as u32, stride);
                                calibration::drawing::draw_chessboard_corners(cal.width, w, h, s, pixels, (cal.columns, cal.rows), &entry.points, true);
                            },
                            _ => { }
                        }
                    }
                }
            }
            //////////////////////////// Draw detected features ////////////////////////////
            
            self.undistortion.write().process_pixels(timestamp_us, width, height, stride, out_width, out_height, out_stride, pixels, out_pixels)
        } else {
            false
        }
    }

    pub fn set_video_rotation(&self, v: f64) { self.params.write().video_rotation = v; }

    pub fn set_trim_start(&self, v: f64) { self.params.write().trim_start = v; }
    pub fn set_trim_end  (&self, v: f64) { self.params.write().trim_end   = v; }

    pub fn set_show_detected_features(&self, v: bool) { self.params.write().show_detected_features = v; }
    pub fn set_show_optical_flow     (&self, v: bool) { self.params.write().show_optical_flow      = v; }
    pub fn set_stab_enabled          (&self, v: bool) { self.params.write().stab_enabled           = v; }
    pub fn set_frame_readout_time    (&self, v: f64)  { self.params.write().frame_readout_time     = v; }
    pub fn set_adaptive_zoom         (&self, v: f64)  { self.params.write().adaptive_zoom_window   = v; }
    pub fn set_fov                   (&self, v: f64)  { self.params.write().fov                    = v; }

    pub fn remove_offset      (&self, timestamp_us: i64)                 { self.gyro.write().remove_offset(timestamp_us); }
    pub fn set_offset         (&self, timestamp_us: i64, offset_ms: f64) { self.gyro.write().set_offset(timestamp_us, offset_ms); }
    pub fn offset_at_timestamp(&self, timestamp_us: i64) -> f64          { self.gyro.read() .offset_at_timestamp(timestamp_us as f64 / 1000.0) }

    pub fn set_imu_lpf(&self, lpf: f64) { self.gyro.write().set_lowpass_filter(lpf); }
    pub fn set_imu_rotation(&self, pitch_deg: f64, roll_deg: f64, yaw_deg: f64) { self.gyro.write().set_imu_rotation(pitch_deg, roll_deg, yaw_deg); }
    pub fn set_imu_orientation(&self, orientation: String) { self.gyro.write().set_imu_orientation(orientation); }
    pub fn set_sync_lpf(&self, lpf: f64) {
        let params = self.params.read();
        self.pose_estimator.lowpass_filter(lpf, params.frame_count, params.duration_ms);
    }

    pub fn set_lens_param(&self, param: &str, value: f64) {
        let mut lens = self.lens.write();
        if lens.fisheye_params.distortion_coeffs.len() >= 4 && 
           lens.fisheye_params.camera_matrix.len() == 3 && 
           lens.fisheye_params.camera_matrix[0].len() == 3 && 
           lens.fisheye_params.camera_matrix[1].len() == 3 && 
           lens.fisheye_params.camera_matrix[2].len() == 3 {
            match param {
                "fx" => lens.fisheye_params.camera_matrix[0][0] = value,
                "fy" => lens.fisheye_params.camera_matrix[1][1] = value,
                "cx" => lens.fisheye_params.camera_matrix[0][2] = value,
                "cy" => lens.fisheye_params.camera_matrix[1][2] = value,
                "k1" => lens.fisheye_params.distortion_coeffs[0] = value,
                "k2" => lens.fisheye_params.distortion_coeffs[1] = value,
                "k3" => lens.fisheye_params.distortion_coeffs[2] = value,
                "k4" => lens.fisheye_params.distortion_coeffs[3] = value,
                _ => { }
            }
        }
    }

    pub fn set_background_color(&self, bg: Vector4<f32>) {
        self.params.write().background = bg;
        self.undistortion.write().set_background(bg);
    }

    pub fn set_smoothing_method(&self, index: usize) -> simd_json::owned::Value {
        let mut smooth = self.smoothing.write();
        smooth.set_current(index);
        smooth.current().get_parameters_json()
    }
    pub fn set_smoothing_param(&self, name: &str, val: f64) {
        self.smoothing.write().current().as_mut().set_parameter(name, val);
    }
    pub fn get_smoothing_algs(&self) -> Vec<String> {
        self.smoothing.read().get_names()
    }

    pub fn get_render_stabilizator(&self, output_size: (usize, usize)) -> StabilizationManager<T> {
        let size = self.params.read().video_size;
        let stab = StabilizationManager {
            params: Arc::new(RwLock::new(self.params.read().clone())),
            gyro:   Arc::new(RwLock::new(self.gyro.read().clone())),
            lens:   Arc::new(RwLock::new(self.lens.read().clone())),
            ..Default::default()
        };
        stab.params.write().framebuffer_inverted = false;
        stab.set_size(size.0, size.1);
        stab.set_output_size(output_size.0, output_size.1);

        stab.recompute_undistortion();

        stab
    }

    pub fn clear(&self) {
        let (stab_enabled, show_detected_features, show_optical_flow, background, adaptive_zoom_window, framebuffer_inverted) = {
            let params = self.params.read();
            (params.stab_enabled, params.show_detected_features, params.show_optical_flow, params.background, params.adaptive_zoom_window, params.framebuffer_inverted)
        };

        *self.params.write() = BasicParams {
            stab_enabled, show_detected_features, show_optical_flow, background, adaptive_zoom_window, framebuffer_inverted, ..Default::default()
        };
        *self.gyro.write() = GyroSource::new();
        self.pose_estimator.clear();
    }

    pub fn override_video_fps(&self, fps: f64) {
        {
            let mut params = self.params.write();
            if (fps - params.fps).abs() > 0.001 {
                params.fps_scale = Some(fps / params.fps);
            } else {
                params.fps_scale = None;
            } 
            self.gyro.write().init_from_params(&params);
        }

        self.undistortion.write().set_compute_params(undistortion::ComputeParams::from_manager(self));

        self.smoothness_checksum.store(0, SeqCst);
        self.adaptive_zoom_checksum.store(0, SeqCst);
    }
}

pub fn timestamp_at_frame(frame: i32, fps: f64) -> f64 { frame as f64 * fps * 1000.0 }
pub fn frame_at_timestamp(timestamp_ms: f64, fps: f64) -> i32 { (timestamp_ms * (fps / 1000.0)).round() as i32 }

pub fn run_threaded<F>(cb: F) where F: FnOnce() + Send + 'static {
    THREAD_POOL.spawn(cb);
}
