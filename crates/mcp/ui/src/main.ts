import { App, applyDocumentTheme, applyHostStyleVariables } from "@modelcontextprotocol/ext-apps";
import { OpenAIExtensions } from "@openai/mcp-extensions/app";
import { Panel } from "./panel.ts";
import { render } from "./view.ts";

const root = document.getElementById("root")!;
const app = new App({ name: "Workers", version: "0.0.0" }, { availableDisplayModes: ["fullscreen"] });
new OpenAIExtensions(app);
const panel = new Panel(app, () => render(root, panel));
function applyContext(): void {
  const context = app.getHostContext();
  if (context?.theme) applyDocumentTheme(context.theme);
  if (context?.styles?.variables) applyHostStyleVariables(context.styles.variables);
  if (context?.locale) document.documentElement.lang = context.locale;
  render(root, panel);
}
app.onhostcontextchanged = applyContext;
render(root, panel);
void panel.connect().then(applyContext);
