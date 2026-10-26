import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import "./App.css";

type Model = { id: string; name: string; provider: string; canServe: boolean };
type Catalog = { models: Model[]; warnings: string[] };
type Provider = { id: string; name: string; runnable: boolean; authentication: string };
type Status = { running: boolean; baseUrl: string | null; accessKey: string | null; provider: string; model: string; lanUrls: string[] };
const cliOptions = [["codex", "Codex"], ["claude", "Claude Code"], ["agy", "AGY"], ["qoder", "Qoder"]] as const;

function App() {
  const [status, setStatus] = useState<Status>({ running: false, baseUrl: null, accessKey: null, provider: "codex", model: "", lanUrls: [] });
  const [models, setModels] = useState<Model[]>([]);
  const [providers, setProviders] = useState<Provider[]>([]);
  const [provider, setProvider] = useState("codex");
  const [apiKey, setApiKey] = useState("");
  const [loading, setLoading] = useState(true);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const [notice, setNotice] = useState("");
  const locked = status.running || busy;
  const availableModels = models.filter(model => model.provider === provider && model.canServe);
  const selectedProvider = providers.find(item => item.id === provider);

  async function refreshModels() {
    setLoading(true); setError("");
    try {
      const [catalog, items] = await Promise.all([invoke<Catalog>("list_models"), invoke<Provider[]>("list_providers")]);
      setModels(catalog.models); setProviders(items);
    } catch { setError("Could not find your CLIs. Please try again."); }
    finally { setLoading(false); }
  }

  useEffect(() => {
    let mounted = true;
    const refresh = () => invoke<Status>("server_status").then(next => {
      if (!mounted) return;
      setStatus(next);
      if (next.running) {
        setApiKey(next.accessKey ?? ""); setProvider(next.provider);
      }
    }).catch(() => {});
    void refresh(); void refreshModels();
    const timer = setInterval(refresh, 1000);
    return () => { mounted = false; clearInterval(timer); };
  }, []);

  async function toggleServer() {
    setBusy(true); setError(""); setNotice("");
    try {
      const next = await invoke<Status>(status.running ? "stop_server" : "start_server", status.running ? {} : {
        port: 9693, accessKey: apiKey, provider, reasoningEffort: null,
      });
      setStatus(next);
    } catch (e) { setError(`Could not ${status.running ? "stop" : "start"} the server. ${String(e)}`); }
    finally { setBusy(false); }
  }

  async function copy(value: string, label: string) {
    try { await navigator.clipboard.writeText(value); setNotice(`${label} copied.`); }
    catch { setError("Could not copy. Select the text and copy it manually."); }
  }

  return <main>
    <header><h1>Sagaryet</h1><span className={`status ${status.running ? "running" : ""}`}><span aria-hidden="true" />{status.running ? "Running" : "Stopped"}</span></header>
    <p className="intro">Your CLI, ready to use in your AI app.</p>

    <section className="setup" aria-label="Server setup">
      <div className="setup-fields">
        <div><label htmlFor="cli">CLI</label><select id="cli" value={provider} disabled={locked || loading} onChange={event => setProvider(event.target.value)}>
          {cliOptions.map(([id, name]) => <option key={id} value={id}>{name}</option>)}
        </select></div>
        <div><label htmlFor="api-key">API key <span className="optional">Optional</span></label><input id="api-key" type="text" value={apiKey} disabled={locked} placeholder="Empty" autoComplete="off" spellCheck={false} onChange={event => setApiKey(event.target.value)} /></div>
      </div>
      {!loading && !status.running && (!selectedProvider?.runnable || selectedProvider.authentication === "reported_signed_out" || !availableModels.length) && <p className="help">{!selectedProvider?.runnable ? `Install ${cliOptions.find(([id]) => id === provider)?.[1]} to continue.` : selectedProvider.authentication === "reported_signed_out" ? "Sign in to your CLI, then refresh." : "This CLI is not supported for serving models yet."}</p>}
      <button className="primary" disabled={busy || (!status.running && (loading || !availableModels.length || !selectedProvider?.runnable || selectedProvider.authentication === "reported_signed_out"))} onClick={toggleServer}>{busy ? "Please wait…" : status.running ? "Stop server" : "Start server"}</button>
    </section>

    <section aria-labelledby="connection-title">
      <h2 id="connection-title">Connection</h2>
      <div className="connection-row"><div><span className="caption">This computer</span><code className={!status.baseUrl ? "placeholder" : ""}>{status.baseUrl || "Available after starting the server"}</code></div><button className="copy" disabled={!status.baseUrl} onClick={() => copy(status.baseUrl!, "API URL")} aria-label="Copy this computer URL">Copy</button></div>
      {status.lanUrls.length ? status.lanUrls.map(url => <div className="connection-row" key={url}><div><span className="caption">Local network</span><code>{url}</code></div><button className="copy" onClick={() => copy(url, "Local network URL")} aria-label="Copy local network URL">Copy</button></div>) : <div className="connection-row"><div><span className="caption">Local network</span><code className="placeholder">{status.running ? "No local network address found" : "Available after starting the server"}</code></div></div>}
      <div className="connection-row"><div><span className="caption">API key</span><code>{(status.running ? status.accessKey : apiKey) || "Empty"}</code></div>{(status.running ? status.accessKey : apiKey) && <button className="copy" onClick={() => copy((status.running ? status.accessKey : apiKey) || "", "API key")} aria-label="Copy API key">Copy</button>}</div>
      <p className="help">{apiKey ? "Use either URL and your key in your AI app." : "No key needed. Anyone on your local network can use the server while no key is set."} Local network access uses unencrypted HTTP.</p>
    </section>

    <section aria-labelledby="models-title">
      <div className="section-heading"><h2 id="models-title">Available models</h2>{!status.running && <button className="text-button" disabled={loading || busy} onClick={refreshModels}>{loading ? "Refreshing…" : "Refresh"}</button>}</div>
      <p className="help model-help">Choose any of these models in your AI app. The API lists all available models for this CLI.</p>
      {loading && !models.length ? <p className="empty">Loading models…</p> : availableModels.length ? <div className="model-list" aria-label="Available models">{availableModels.map(model => <div className="active-model" key={model.id}><div><strong>{model.name}</strong><code>{model.id}</code></div><button className="copy" onClick={() => copy(model.id, "Model name")}>Copy</button></div>)}</div> : <p className="empty">No models available.</p>}
    </section>

    <details><summary>API keys & fields</summary><p className="help">For apps that ask for API settings or request fields.</p><dl className="api-fields"><div><dt>base_url</dt><dd>{status.baseUrl || "Shown when the server starts"}</dd></div><div><dt>api_key</dt><dd>{apiKey ? "Your API key above" : "Empty"}</dd></div><div><dt>model</dt><dd>Any model ID from the available models list</dd></div><div><dt>messages</dt><dd>Required · Your conversation</dd></div><div><dt>stream</dt><dd>Optional · true or false</dd></div></dl></details>
    <div className="feedback" role="status" aria-live="polite">{notice}</div>{error && <p role="alert" className="error">{error}</p>}
  </main>;
}
export default App;
