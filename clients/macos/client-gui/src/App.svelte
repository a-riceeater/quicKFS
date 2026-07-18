<script lang="ts">
  import { onMount } from "svelte";
  import { invoke } from "@tauri-apps/api/core";
  import {
    PAIRING_CODE_LENGTH,
    normalizePairingCode,
    splitPairingCode
  } from "./lib/pairing-code";
  import {
    THEME_STORAGE_KEY,
    applyTheme,
    isThemePreference,
    type ThemePreference
  } from "./lib/theme";

  type Bootstrap = {
    macfuseInstalled: boolean;
    macfuseInstallUrl: string;
    maxClientReadSize: number;
    platform: string;
  };

  type StartupState = "checking" | "ready" | "macfuse-missing" | "error";

  const themeOptions: Array<{ value: ThemePreference; label: string }> = [
    { value: "system", label: "System" },
    { value: "light", label: "Light" },
    { value: "dark", label: "Dark" }
  ];

  let themePreference: ThemePreference = "system";
  let pairingCode = "";
  let codeInput: HTMLInputElement | undefined;
  let submissionNote = "";
  let bootstrap: Bootstrap | null = null;
  let startupState: StartupState = "checking";
  let startupError = "";
  let installActionError = "";
  let macfuseInstallUrl = "https://macfuse.io/";

  $: pairingGroups = splitPairingCode(pairingCode);
  $: codeComplete = pairingCode.length === PAIRING_CODE_LENGTH;
  $: activeIndex = codeComplete ? -1 : pairingCode.length;

  function updateTheme(preference: ThemePreference): void {
    themePreference = preference;
    localStorage.setItem(THEME_STORAGE_KEY, preference);
    applyTheme(preference, window.matchMedia("(prefers-color-scheme: dark)").matches);
  }

  function selectTheme(event: Event): void {
    const value = (event.currentTarget as HTMLSelectElement).value;
    if (isThemePreference(value)) updateTheme(value);
  }

  function updatePairingCode(event: Event): void {
    const target = event.currentTarget as HTMLInputElement;
    pairingCode = normalizePairingCode(target.value);
    target.value = pairingCode;
    submissionNote = "";
  }

  function focusPairingCode(): void {
    codeInput?.focus({ preventScroll: true });
  }

  function continuePairing(): void {
    if (!codeComplete) {
      focusPairingCode();
      return;
    }
    codeInput?.blur();
    submissionNote = "Code format complete. Ready for server verification.";
  }

  function openMacfuseInstallPage(event: MouseEvent): void {
    event.preventDefault();
    installActionError = "";

    if (window.__TAURI_INTERNALS__) {
      void invoke<void>("open_macfuse_install_page").catch(() => {
        installActionError = `Open ${macfuseInstallUrl} in your browser.`;
      });
      return;
    }

    window.open(macfuseInstallUrl, "_blank", "noopener,noreferrer");
  }

  onMount(() => {
    let disposed = false;
    let focusTimer: number | undefined;
    const stored = localStorage.getItem(THEME_STORAGE_KEY);
    themePreference = isThemePreference(stored) ? stored : "system";

    const media = window.matchMedia("(prefers-color-scheme: dark)");
    const syncSystemTheme = () => applyTheme(themePreference, media.matches);
    syncSystemTheme();
    media.addEventListener("change", syncSystemTheme);

    const showPairing = () => {
      startupState = "ready";
      focusTimer = window.setTimeout(focusPairingCode, 160);
    };

    if (!window.__TAURI_INTERNALS__) {
      showPairing();
    } else {
      void invoke<Bootstrap>("frontend_bootstrap")
        .then((value) => {
          if (disposed) return;
          bootstrap = value;
          macfuseInstallUrl = value.macfuseInstallUrl;
          if (value.macfuseInstalled) {
            showPairing();
          } else {
            startupState = "macfuse-missing";
          }
        })
        .catch(() => {
          if (disposed) return;
          startupState = "error";
          startupError = "quicKFS could not verify this Mac's local requirements.";
        });
    }

    return () => {
      disposed = true;
      if (focusTimer !== undefined) window.clearTimeout(focusTimer);
      media.removeEventListener("change", syncSystemTheme);
    };
  });
</script>

<svelte:head>
  <meta name="description" content="Pair this Mac with a quicKFS server." />
</svelte:head>

<main class="app-shell">
  <div class="content-area">
    {#if startupState === "checking"}
      <section class="message-view checking-view" aria-labelledby="startup-title" aria-live="polite">
        <span class="spinner" aria-hidden="true"></span>
        <h1 id="startup-title">Checking this Mac…</h1>
      </section>
    {:else if startupState === "macfuse-missing"}
      <section class="message-view" aria-labelledby="dependency-title">
        <h1 id="dependency-title">macFUSE is required</h1>
        <p>Install macFUSE, then reopen quicKFS.</p>
        <div class="message-actions">
          <a
            class="primary-button install-button"
            href={macfuseInstallUrl}
            target="_blank"
            rel="noopener noreferrer external"
            onclick={openMacfuseInstallPage}
          >Install macFUSE</a>
        </div>
        {#if installActionError}
          <p class="inline-error" role="alert">{installActionError}</p>
        {/if}
      </section>
    {:else if startupState === "error"}
      <section class="message-view" aria-labelledby="startup-error-title">
        <h1 id="startup-error-title">Startup check failed</h1>
        <p>{startupError} Reopen the app to try again.</p>
      </section>
    {:else}
      <section class="pairing-view" aria-labelledby="pairing-title">
        <h1 id="pairing-title">Enter pairing code</h1>
        <p id="pairing-help" class="visually-hidden">
          Enter or paste the case-sensitive 27-character code shown by your quicKFS server.
        </p>

        <label
          class="code-entry"
          class:complete={codeComplete}
          aria-label={`Pairing code: ${pairingCode.length} of ${PAIRING_CODE_LENGTH} characters entered`}
          aria-describedby="pairing-help pairing-progress"
          for="pairing-code"
        >
          <input
            id="pairing-code"
            class="code-capture"
            bind:this={codeInput}
            value={pairingCode}
            maxlength="33"
            autocomplete="one-time-code"
            autocapitalize="none"
            spellcheck="false"
            aria-label="Pairing code"
            oninput={updatePairingCode}
          />

          <span class="code-groups" aria-hidden="true">
            {#each pairingGroups as group, groupIndex}
              <span class="code-group">
                {#each group as character, characterIndex}
                  {@const index = groupIndex * 4 + characterIndex}
                  <span
                    class="code-slot"
                    class:filled={character !== " "}
                    class:current={index === activeIndex}
                  >
                    <span class="code-character">{character === " " ? "\u00a0" : character}</span>
                    <span class="code-underline"></span>
                  </span>
                {/each}
              </span>
            {/each}
          </span>
        </label>

        <div class="form-footer">
          <span id="pairing-progress" class="progress-copy" aria-live="polite">
            {pairingCode.length} of {PAIRING_CODE_LENGTH}
          </span>
          <button
            class="primary-button continue-button"
            class:ready={codeComplete}
            type="button"
            disabled={!codeComplete}
            onclick={continuePairing}
          >Continue</button>
        </div>

        {#if submissionNote}
          <p class="submission-note" role="status">{submissionNote}</p>
        {/if}
      </section>
    {/if}
  </div>

  <footer class="status-bar">
    <span
      class="status-copy"
      title={bootstrap ? `${bootstrap.platform} · ${bootstrap.maxClientReadSize} byte read limit` : undefined}
    >
      <span
        class="status-dot"
        class:ready={startupState === "ready"}
        class:warning={startupState === "macfuse-missing"}
        class:error={startupState === "error"}
      ></span>
      {#if startupState === "checking"}
        Checking requirements
      {:else if startupState === "macfuse-missing"}
        macFUSE not installed
      {:else if startupState === "error"}
        Startup unavailable
      {:else}
        Ready to pair
      {/if}
    </span>

    <label class="appearance-control">
      <span>Appearance</span>
      <select aria-label="Appearance" value={themePreference} onchange={selectTheme}>
        {#each themeOptions as option}
          <option value={option.value}>{option.label}</option>
        {/each}
      </select>
    </label>
  </footer>
</main>
