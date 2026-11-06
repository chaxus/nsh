import test from "node:test";
import assert from "node:assert/strict";
import { Values } from "./values.js";
import { createDom } from "./dom.js";

/** A MediaStream stand-in: only `getTracks` is what the facade checks. */
function fakeStream() {
  return { getTracks: () => [] };
}

test("Video stream prop attaches a MediaStream through the player side channel", () => {
  const values = new Values();
  const calls = [];
  const streams = [];
  const player = {
    setVideoStream(id, stream) {
      streams.push([id, stream]);
    },
  };
  const dom = createDom(
    (op, args) => calls.push([op, args]),
    values,
    { getOwner: () => null },
    () => true,
    () => {},
    player,
  );
  const video = values.makeNode(7);
  const stream = fakeStream();

  dom.rt.prop(video, "stream", stream);
  assert.deepEqual(streams, [[7, stream]]);
  // Clearing the URL src so it does not fight the stream.
  assert.deepEqual(
    calls.filter(([op]) => op === 303).map(([, [, name, value]]) => [name, value]),
    [["src", null]],
  );

  // Detach.
  streams.length = 0;
  calls.length = 0;
  dom.rt.prop(video, "stream", null);
  assert.deepEqual(streams, [[7, null]]);
  assert.deepEqual(calls.filter(([op]) => op === 303), []);
});

test("Video stream prop ignores a value that is not a MediaStream-shaped object", () => {
  const values = new Values();
  const streams = [];
  const player = {
    setVideoStream(id, stream) {
      streams.push([id, stream]);
    },
  };
  const dom = createDom(
    () => {},
    values,
    { getOwner: () => null },
    () => true,
    () => {},
    player,
  );
  const video = values.makeNode(3);
  dom.rt.prop(video, "stream", { notAStream: true });
  assert.deepEqual(streams, [[3, null]]);
});
