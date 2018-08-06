use alpha::MAX_CHANNELS;
use gamma::ToneCurve;
use internal::quick_saturate_word;
use pcs::{lab_to_xyz, xyz_to_lab, MAX_ENCODEABLE_XYZ};
use std::fmt;
use transform::NamedColorList;
use {CIELab, CIEXYZ};

type StageEvalFn = fn(&[f32], &mut [f32], &Stage);

/// Multi process elements types
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StageType {
    /// `cvst`
    CurveSet = 0x63767374,
    /// `matf`
    Matrix = 0x6D617466,
    /// `clut`
    CLut = 0x636C7574,

    /// `bACS`
    BAcs = 0x62414353,
    /// `eACS`
    EAcs = 0x65414353,

    // Custom from here, not in the ICC Spec
    /// `l2x `
    XYZ2Lab = 0x6C327820,
    /// `x2l `
    Lab2XYZ = 0x78326C20,
    /// `ncl `
    NamedColor = 0x6E636C20,
    /// `2 4 `
    LabV2toV4 = 0x32203420,
    /// `4 2 `
    LabV4toV2 = 0x34203220,

    // Identities
    /// `idn `
    Identity = 0x69646E20,

    // Float to floatPCS
    /// `d2l `
    Lab2FloatPCS = 0x64326C20,
    /// `l2d `
    FloatPCS2Lab = 0x6C326420,
    /// `d2x `
    XYZ2FloatPCS = 0x64327820,
    /// `x2d `
    FloatPCS2XYZ = 0x78326420,
    /// `clp `
    ClipNegatives = 0x636c7020,
}

#[derive(Debug, Clone)]
pub(crate) enum StageData {
    None,
    Matrix {
        matrix: Vec<f64>,
        offset: Option<Vec<f64>>,
    },
    Curves(Vec<ToneCurve>),
    NamedColorList(NamedColorList),
}

#[derive(Clone)]
pub struct Stage {
    ty: StageType,
    implements: StageType,

    input_channels: u32,
    output_channels: u32,

    eval_fn: StageEvalFn,

    data: StageData,
}

impl Stage {
    pub(crate) fn alloc(
        ty: StageType,
        input_channels: u32,
        output_channels: u32,
        eval_fn: StageEvalFn,
        data: StageData,
    ) -> Stage {
        Stage {
            ty,
            implements: ty,
            input_channels,
            output_channels,
            eval_fn,
            data,
        }
    }

    // Curves == NULL forces identity curves
    pub(crate) fn new_tone_curves(channels: u32, curves: Option<&[ToneCurve]>) -> Stage {
        let curves = if let Some(curves) = curves {
            curves.to_vec()
        } else {
            let mut curves = Vec::new();
            for _ in 0..channels {
                curves.push(ToneCurve::new_gamma(1.).unwrap());
            }
            curves
        };

        Stage::alloc(
            StageType::CurveSet,
            channels,
            channels,
            evaluate_curves,
            StageData::Curves(curves),
        )
    }

    pub(crate) fn new_matrix(
        rows: u32,
        cols: u32,
        matrix: &[f64],
        offset: Option<&[f64]>,
    ) -> Stage {
        Self::alloc(
            StageType::Matrix,
            cols,
            rows,
            evaluate_matrix,
            StageData::Matrix {
                matrix: matrix.to_vec(),
                offset: offset.map(|x| x.to_vec()),
            },
        )
    }

    pub(crate) fn new_labv2_to_v4() -> Stage {
        const V2_TO_V4: [f64; 9] = [
            65535. / 65280.,
            0.,
            0.,
            0.,
            65535. / 65280.,
            0.,
            0.,
            0.,
            65535. / 65280.,
        ];

        let mut stage = Self::new_matrix(3, 3, &V2_TO_V4, None);
        stage.implements = StageType::LabV2toV4;

        stage
    }

    pub(crate) fn new_labv4_to_v2() -> Stage {
        const V4_TO_V2: [f64; 9] = [
            65280. / 65535.,
            0.,
            0.,
            0.,
            65280. /65535.,
            0.,
            0.,
            0.,
            65280. /65535.,
        ];

        let mut stage = Self::new_matrix(3, 3, &V4_TO_V2, None);
        stage.implements = StageType::LabV4toV2;

        stage
    }

    pub(crate) fn new_xyz_to_lab() -> Stage {
        Self::alloc(
            StageType::XYZ2Lab,
            3,
            3,
            evaluate_xyz_to_lab,
            StageData::None,
        )
    }

    pub(crate) fn new_lab_to_xyz() -> Stage {
        Self::alloc(
            StageType::Lab2XYZ,
            3,
            3,
            evaluate_lab_to_xyz,
            StageData::None,
        )
    }

    pub(crate) fn new_clip_negatives(channels: u32) -> Stage {
        Self::alloc(
            StageType::ClipNegatives,
            channels,
            channels,
            clipper,
            StageData::None,
        )
    }

    /// From Lab to float. Note that the MPE gives numbers in normal Lab range and we need the
    /// 0..1.0 range for the formatters.
    /// L*:   0...100 => 0...1.0  (L* / 100)
    /// ab*: -128..+127 to 0..1   ((ab* + 128) / 255)
    pub(crate) fn new_normalize_from_lab_float() -> Stage {
        const A1: [f64; 9] = [1. / 100., 0., 0., 0., 1. / 255., 0., 0., 0., 1. / 255.];

        const O1: [f64; 3] = [0., 128. / 255., 128. / 255.];

        let mut stage = Self::new_matrix(3, 3, &A1, Some(&O1));
        stage.implements = StageType::Lab2FloatPCS;
        stage
    }

    /// From XYZ to floating point PCS
    pub(crate) fn new_normalize_from_xyz_float() -> Stage {
        const A1: [f64; 9] = [
            32768. / 65535.,
            0.,
            0.,
            0.,
            32768. / 65535.,
            0.,
            0.,
            0.,
            32768. / 65535.,
        ];

        let mut stage = Self::new_matrix(3, 3, &A1, None);
        stage.implements = StageType::XYZ2FloatPCS;
        stage
    }

    pub(crate) fn new_normalize_to_lab_float() -> Stage {
        const A1: [f64; 9] = [100., 0., 0., 0., 255., 0., 0., 0., 255.];

        const O1: [f64; 3] = [0., -128., -128.];

        let mut stage = Self::new_matrix(3, 3, &A1, Some(&O1));
        stage.implements = StageType::FloatPCS2Lab;
        stage
    }

    pub(crate) fn new_normalize_to_xyz_float() -> Stage {
        const A1: [f64; 9] = [
            65535. / 32768.,
            0.,
            0.,
            0.,
            65535. / 32768.,
            0.,
            0.,
            0.,
            65535. / 32768.,
        ];

        let mut stage = Self::new_matrix(3, 3, &A1, None);
        stage.implements = StageType::FloatPCS2XYZ;
        stage
    }

    pub(crate) fn new_identity(channels: u32) -> Stage {
        Stage::alloc(StageType::Identity, channels, channels, evaluate_identity, StageData::None)
    }

    /// Creates a bunch of identity curves.
    pub(crate) fn new_identity_curves(channels: u32) -> Stage {
        let mut stage = Stage::new_tone_curves(channels, None);
        stage.implements = StageType::Identity;
        stage
    }
}

impl fmt::Debug for Stage {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "Stage {{ type: {:?}, impl: {:?}, channels: {} -> {}, data: {:?} }}",
            self.ty, self.implements, self.input_channels, self.output_channels, self.data
        )
    }
}

type OPTeval16Fn = fn(&[u16], &mut [u16], &Pipeline);

/// Pipeline evaluator (in floating point)
type PipelineEvalFloatFn = fn(&[f32], &mut [f32], &Pipeline);

#[derive(Clone)]
pub struct Pipeline {
    elements: Vec<Stage>,
    pub input_channels: u32,
    pub output_channels: u32,

    data: (),
    eval_16_fn: OPTeval16Fn,
    eval_float_fn: PipelineEvalFloatFn,
    save_as_8_bits: bool,
}

impl Pipeline {
    pub fn alloc(input_channels: u32, output_channels: u32) -> Pipeline {
        // A value of zero in channels is allowed as placeholder
        if input_channels >= MAX_CHANNELS as u32 || output_channels >= MAX_CHANNELS as u32 {
            panic!("Pipeline: too many channels");
        }

        let mut lut = Pipeline {
            elements: Vec::new(),
            input_channels,
            output_channels,
            eval_16_fn: lut_eval_16,
            eval_float_fn: lut_eval_float,
            data: (),
            save_as_8_bits: false,
        };

        lut.bless();

        lut
    }

    fn bless(&mut self) {
        // We can set the input/output channels only if we have elements.
        if !self.elements.is_empty() {
            let first = self.elements.first().unwrap();
            let last = self.elements.last().unwrap();

            self.input_channels = first.input_channels;
            self.output_channels = last.output_channels;

            // don’t need to check chain consistency
        }
    }

    pub fn eval_16(&self, input: &[u16], output: &mut [u16]) {
        (self.eval_16_fn)(input, output, self);
    }

    // Evaluates the LUT with f32.
    pub fn eval_float(&self, input: &[f32], output: &mut [f32]) {
        (self.eval_float_fn)(input, output, self);
    }

    pub(crate) fn prepend_stage(&mut self, stage: Stage) {
        self.elements.insert(0, stage);
        self.bless();
    }

    pub(crate) fn append_stage(&mut self, stage: Stage) {
        self.elements.push(stage);
        self.bless();
    }

    /// Concatenate two LUT into a new single one
    pub(crate) fn concat(&mut self, other: &Pipeline) {
        // If both LUTS does not have elements, we need to inherit
        // the number of channels
        if self.elements.is_empty() && other.elements.is_empty() {
            self.input_channels = other.input_channels;
            self.output_channels = other.output_channels;
        }

        // Cat second
        for stage in &other.elements {
            self.elements.push(stage.clone());
        }

        self.bless();
    }
}

impl fmt::Debug for Pipeline {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "Pipeline {{ elements: {:?}, channels: {} -> {}, save as 8: {:?} }}",
            self.elements, self.input_channels, self.output_channels, self.save_as_8_bits
        )
    }
}

/// From floating point to 16 bits
fn from_float_to_16(input: &[f32], output: &mut [u16], n: usize) {
    for i in 0..n {
        output[i] = quick_saturate_word((input[i] * 65535.).into());
    }
}

/// From 16 bits to floating point
fn from_16_to_float(input: &[u16], output: &mut [f32]) {
    for i in 0..input.len() {
        output[i] = input[i] as f32 / 65535.;
    }
}

fn copy_float_slice(src: &[f32], dest: &mut [f32]) {
    if src.len() > dest.len() {
        let dest_len = dest.len();
        dest.copy_from_slice(&src[0..dest_len]);
    } else {
        dest[0..src.len()].copy_from_slice(src);
    }
}

const MAX_STAGE_CHANNELS: usize = 128;

/// Default function for evaluating the LUT with 16 bits. Precision is retained.
fn lut_eval_16(input: &[u16], output: &mut [u16], pipeline: &Pipeline) {
    let mut phase = 0;
    let mut storage = [[0.; MAX_STAGE_CHANNELS], [0.; MAX_STAGE_CHANNELS]];
    from_16_to_float(input, &mut storage[phase]);

    for stage in &pipeline.elements {
        let next_phase = phase ^ 1;
        let next_phase_yes_this_is_safe =
            unsafe { &mut *(&storage[next_phase] as *const _ as *mut [f32; MAX_STAGE_CHANNELS]) };
        (stage.eval_fn)(&storage[phase], next_phase_yes_this_is_safe, &stage);
        phase = next_phase;
    }

    from_float_to_16(&storage[phase], output, pipeline.output_channels as usize);
}

/// Evaluates the LUt with floats.
fn lut_eval_float(input: &[f32], output: &mut [f32], pipeline: &Pipeline) {
    let mut phase = 0;
    let mut storage = [[0.; MAX_STAGE_CHANNELS], [0.; MAX_STAGE_CHANNELS]];

    copy_float_slice(input, &mut storage[phase]);

    for stage in &pipeline.elements {
        let next_phase = phase ^ 1;
        let next_phase_yes_this_is_safe =
            unsafe { &mut *(&storage[next_phase] as *const _ as *mut [f32; MAX_STAGE_CHANNELS]) };
        (stage.eval_fn)(&storage[phase], next_phase_yes_this_is_safe, &stage);
        phase = next_phase;
    }

    copy_float_slice(&storage[phase], output);
}

/// Special care should be taken here because precision loss. A temporary cmsFloat64Number buffer is being used
fn evaluate_matrix(input: &[f32], output: &mut [f32], stage: &Stage) {
    let (matrix, offset) = match stage.data {
        StageData::Matrix {
            ref matrix,
            ref offset,
        } => (matrix, offset),
        _ => panic!("Invalid stage data (this shouldn’t happen)"),
    };

    // Input is already in 0..1.0 notation
    for i in 0..stage.output_channels {
        let mut tmp = 0.;
        for j in 0..stage.input_channels {
            tmp += input[j as usize] as f64 * matrix[(i * stage.input_channels + j) as usize];
        }
        if let Some(offset) = offset {
            tmp += offset[i as usize];
        }
        output[i as usize] = tmp as f32;
    }
    // Output in 0..1.0 domain
}

fn evaluate_curves(input: &[f32], output: &mut [f32], stage: &Stage) {
    let curves = match stage.data {
        StageData::Curves(ref c) => c,
        _ => panic!("Invalid stage data (this shouldn’t happen)"),
    };

    for i in 0..curves.len() {
        output[i] = curves[i].eval_float(input[i]);
    }
}

fn evaluate_xyz_to_lab(input: &[f32], output: &mut [f32], _: &Stage) {
    // From 0..1.0 to XYZ
    let xyz = CIEXYZ {
        x: input[0] as f64 * MAX_ENCODEABLE_XYZ,
        y: input[1] as f64 * MAX_ENCODEABLE_XYZ,
        z: input[2] as f64 * MAX_ENCODEABLE_XYZ,
    };

    let lab = xyz_to_lab(None, xyz);

    // From V4 Lab to 0..1.0
    output[0] = (lab.L / 100.) as f32;
    output[1] = ((lab.a + 128.) / 255.) as f32;
    output[2] = ((lab.b + 128.) / 255.) as f32;
}

fn evaluate_lab_to_xyz(input: &[f32], output: &mut [f32], _: &Stage) {
    // V4 rules
    let lab = CIELab {
        L: input[0] as f64 * 100.,
        a: input[1] as f64 * 255. - 128.,
        b: input[2] as f64 * 255. - 128.,
    };

    let xyz = lab_to_xyz(None, lab);

    // From XYZ, range 0..19997 to 0..1.0, note that 1.99997 comes from 0xffff
    // encoded as 1.15 fixed point, so 1 + (32767.0 / 32768.0)

    output[0] = (xyz.x / MAX_ENCODEABLE_XYZ) as f32;
    output[1] = (xyz.y / MAX_ENCODEABLE_XYZ) as f32;
    output[2] = (xyz.z / MAX_ENCODEABLE_XYZ) as f32;
}

/// Clips values smaller than zero
fn clipper(input: &[f32], output: &mut [f32], stage: &Stage) {
    for i in 0..stage.input_channels as usize {
        output[i] = input[i].max(0.);
    }
}

fn evaluate_identity(input: &[f32], output: &mut [f32], _: &Stage) {
    copy_float_slice(input, output);
}