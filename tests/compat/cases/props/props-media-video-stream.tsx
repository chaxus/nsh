// A case of the API's test suite (docs/dev/compatibility.md): a video given
// a live stream (`srcObject`) beside its `src`. The web plays the stream;
// here, as on the native platforms, the video keeps its other props and the
// app runs on as the stream is set, replaced and cleared. Its script checks it.
import { Text, Video, View, signal } from "nsh";

// The engine never sees the stream: any object stands in for one here.
const live = () => ({ id: "compat", active: true }) as MediaStream;

function App() {
  const stream = signal<MediaStream | null>(live());
  return (
    <View style={{ flex: 1, paddingTop: 64 }}>
      <View testID="swap" onPress={() => stream.set(stream() ? null : live())}>
        <Text>{stream() ? "Live" : "Stopped"}</Text>
      </View>
      <Video
        testID="live"
        src="https://example.com/fallback.mp4"
        srcObject={stream()}
        muted
        style={{ width: 160, height: 90 }}
      />
    </View>
  );
}

export default App;
