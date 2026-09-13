use arcstr::ArcStr;
use smallvec::SmallVec;

pub type SmallVecArcStr = SmallVec<[ArcStr; 8]>;
pub type SmallMatrixArcStr = SmallVec<[SmallVecArcStr; 8]>;
