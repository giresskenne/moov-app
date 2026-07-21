import assert from "node:assert/strict";
import test from "node:test";

// main.js only registers init here; it does not touch the DOM until the event
// fires, so a small window stub lets us test the exact production pose logic.
globalThis.window = {
  tf: undefined,
  poseDetection: undefined,
  addEventListener() {},
};

const pose = await import("../src/main.js");

const p = (x, y) => ({ x, y, score: 0.99 });

function frontPose() {
  return {
    left_shoulder: p(100, 100),
    right_shoulder: p(200, 100),
    left_hip: p(110, 200),
    right_hip: p(190, 200),
    left_knee: p(120, 300),
    right_knee: p(180, 300),
    left_wrist: p(140, 110),
    right_wrist: p(160, 110),
  };
}

function profilePose() {
  return {
    left_shoulder: p(150, 100),
    left_hip: p(150, 200),
    left_knee: p(150, 300),
  };
}

test("standing side bend recognizes centre and a comfortable side shift", () => {
  const start = frontPose();
  const bent = frontPose();
  bent.left_shoulder.x += 25;
  bent.right_shoulder.x += 25;

  assert.equal(pose.sideBendCentre(start), true);
  assert.equal(pose.sideBendSide(start), false);
  assert.equal(pose.sideBendSide(bent), true);
});

test("hip hinge accepts a profile pose and distinguishes tall from forward", () => {
  const start = profilePose();
  const hinged = profilePose();
  hinged.left_shoulder = p(190, 150);

  assert.equal(pose.hipHingeGate(start), true);
  assert.equal(pose.hipHingeTall(start), true);
  assert.equal(pose.hipHingeForward(start), false);
  assert.equal(pose.hipHingeForward(hinged), true);
});

test("march recognizes both-knees-down and one controlled knee lift", () => {
  const start = frontPose();
  const lifted = frontPose();
  lifted.left_knee = p(120, 225);

  assert.equal(pose.marchGate(start), true);
  assert.equal(pose.marchFeetDown(start), true);
  assert.equal(pose.marchKneeUp(start), false);
  assert.equal(pose.marchKneeUp(lifted), true);
});

test("arm opening recognizes hands together and a shoulder-height T", () => {
  const closed = frontPose();
  const open = frontPose();
  open.left_wrist = p(55, 110);
  open.right_wrist = p(245, 110);

  assert.equal(pose.armSwingGate(closed), true);
  assert.equal(pose.armSwingClosed(closed), true);
  assert.equal(pose.armSwingOpen(closed), false);
  assert.equal(pose.armSwingOpen(open), true);
});

test("range evaluator requires two stable frames and a full reset per rep", () => {
  // March is currently commented out of STRETCHES, so exercise the reusable
  // evaluator with the retained March pose primitives instead of enabling it.
  const disabledMarchFixture = {
    gate: pose.marchGate,
    low: pose.marchFeetDown,
    high: pose.marchKneeUp,
    cue: "Lift one knee",
  };
  const evaluator = new pose.RangeRepEvaluator(disabledMarchFixture, 2);
  const start = frontPose();
  const lifted = frontPose();
  lifted.left_knee = p(120, 225);

  evaluator.update(start, 120);
  evaluator.update(start, 120);
  evaluator.update(lifted, 120);
  let result = evaluator.update(lifted, 120);
  assert.equal(result.debug.reps, 1);

  // Holding the knee up cannot create extra reps.
  result = evaluator.update(lifted, 120);
  assert.equal(result.debug.reps, 1);

  evaluator.update(start, 120);
  evaluator.update(start, 120);
  evaluator.update(lifted, 120);
  result = evaluator.update(lifted, 120);
  assert.equal(result.debug.reps, 2);
  assert.equal(result.done, true);
});
