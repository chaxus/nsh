// A case of the API's test suite (docs/dev/compatibility.md): a video given
// a live stream the embedding app registered (`srcObject`,
// `MediaStream.fromHost`) beside its `src`: the handle's name, the
// stream set, cleared and set again; a name that is not one is a
// `TypeError`. Its script checks them (the platform's side of `srcObject`
// once nsh-compat reads it).
import { MediaStream, Text, Video, View, signal } from "nsh";

function App() {
  const live = signal(true);
  let wrong = "none";
  try {
    MediaStream.fromHost("");
  } catch (error) {
    wrong = error instanceof TypeError ? "TypeError" : String(error);
  }
  const stream = MediaStream.fromHost("compat-camera");
  return (
    <View style={{ flex: 1, paddingTop: 64 }}>
      <View testID="swap" onPress={() => live.set(!live())}>
        <Text>{`${live() ? "Live" : "Stopped"} ${stream.name}, empty ${wrong}`}</Text>
      </View>
      <Video
        testID="live"
        src="https://example.com/fallback.mp4"
        srcObject={live() ? stream : null}
        muted
        style={{ width: 160, height: 90 }}
      />
    </View>
  );
}

export default App;
