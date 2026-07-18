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

  function themeIcon(theme: ThemePreference): string {
    if (theme === "light") return "sun";
    if (theme === "dark") return "moon";
    return "system";
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
      focusTimer = window.setTimeout(focusPairingCode, 420);
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
  <meta
    name="description"
    content="Pair this Mac securely with a quicKFS server."
  />
</svelte:head>

<main class="app-shell">
  <header class="topbar" aria-label="Application controls">
    <a class="brand" href="/" aria-label="quicKFS home">
      <svg class="brand-mark" viewBox="0 0 32 32" aria-hidden="true">
        <path d="M8.5 8.5h9l6 6v9h-15z" />
        <path d="M17.5 8.5v6h6" />
        <path d="M5.5 12.5v13h13" />
      </svg>
      <span>quicKFS</span>
    </a>

    <div class="theme-switch" role="group" aria-label="Appearance">
      {#each themeOptions as option}
        <button
          class:active={themePreference === option.value}
          type="button"
          aria-label={`${option.label} appearance`}
          aria-pressed={themePreference === option.value}
          title={`${option.label} appearance`}
          onclick={() => updateTheme(option.value)}
        >
          {#if themeIcon(option.value) === "sun"}
            <svg viewBox="0 0 20 20" aria-hidden="true">
              <circle cx="10" cy="10" r="3.25" />
              <path d="M10 2v1.5M10 16.5V18M2 10h1.5M16.5 10H18M4.35 4.35l1.06 1.06M14.59 14.59l1.06 1.06M15.65 4.35l-1.06 1.06M5.41 14.59l-1.06 1.06" />
            </svg>
          {:else if themeIcon(option.value) === "moon"}
            <svg viewBox="0 0 20 20" aria-hidden="true">
              <path d="M16.2 12.65A7 7 0 0 1 7.35 3.8a7 7 0 1 0 8.85 8.85Z" />
            </svg>
          {:else}
            <svg viewBox="0 0 20 20" aria-hidden="true">
              <rect x="3" y="4" width="14" height="10" rx="1.75" />
              <path d="M7.5 17h5M10 14v3" />
            </svg>
          {/if}
          <span>{option.label}</span>
        </button>
      {/each}
    </div>
  </header>

  {#if startupState === "checking"}
    <section class="startup-stage" aria-labelledby="startup-title" aria-live="polite">
      <div class="startup-indicator enter-one" aria-hidden="true">
        <span></span>
      </div>
      <div class="eyebrow enter-two">Startup check</div>
      <h1 id="startup-title" class="enter-three">Preparing this Mac</h1>
      <p class="lede enter-four">Checking for the local components quicKFS needs.</p>
    </section>
  {:else if startupState === "macfuse-missing"}
    <section class="dependency-stage" aria-labelledby="dependency-title">
      <div class="dependency-icon enter-one" aria-hidden="true">
        <svg viewBox="0 0 32 32">
          <path d="M7.5 9.5h17v13h-17z" />
          <path d="M11 13.5h10M11 18.5h6" />
          <path d="M12 6.5h8M12 25.5h8" />
        </svg>
      </div>
      <div class="eyebrow enter-two">Required component</div>
      <h1 id="dependency-title" class="enter-three">Install macFUSE to continue</h1>
      <p class="lede dependency-copy enter-four">
        quicKFS uses macFUSE to present remote files in Finder. Pairing and client
        commands are paused until it is installed.
      </p>
      <div class="dependency-actions enter-five">
        <a
          class="install-button"
          href={macfuseInstallUrl}
          target="_blank"
          rel="noopener noreferrer external"
          onclick={openMacfuseInstallPage}
        >
          <span>Get macFUSE</span>
          <svg viewBox="0 0 20 20" aria-hidden="true">
            <path d="M7 13 13.5 6.5M8.5 6.5h5v5" />
            <path d="M13 11.5V15H5V7h3.5" />
          </svg>
        </a>
        <a
          class="install-url"
          href={macfuseInstallUrl}
          target="_blank"
          rel="noopener noreferrer external"
          onclick={openMacfuseInstallPage}
        >{macfuseInstallUrl}</a>
      </div>
      <p class="dependency-hint enter-six">
        Complete any macOS approval prompts, then quit and reopen quicKFS.
      </p>
      {#if installActionError}
        <p class="install-action-error" role="alert">{installActionError}</p>
      {/if}
    </section>
  {:else if startupState === "error"}
    <section class="dependency-stage" aria-labelledby="startup-error-title">
      <div class="eyebrow enter-one">Startup interrupted</div>
      <h1 id="startup-error-title" class="enter-two">Couldn’t finish the startup check</h1>
      <p class="lede enter-three">{startupError} Quit and reopen the app to try again.</p>
    </section>
  {:else}
    <section class="pairing-stage" aria-labelledby="pairing-title">
    <div class="eyebrow enter-one">Secure pairing</div>
    <h1 id="pairing-title" class="enter-two">Enter your pairing code</h1>
    <p id="pairing-help" class="lede enter-three">
      Type or paste the one-time code shown by your quicKFS server. It is case-sensitive
      and expires shortly after it is created.
    </p>

    <label
      class="code-entry enter-four"
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

    <div id="pairing-progress" class="progress-copy enter-five" aria-live="polite">
      <span>{pairingCode.length} / {PAIRING_CODE_LENGTH}</span>
      <span aria-hidden="true">·</span>
      <span>Spaces and grouping separators paste automatically</span>
    </div>

    <div class="action-row enter-six">
      <button
        class="continue-button"
        class:ready={codeComplete}
        type="button"
        disabled={!codeComplete}
        onclick={continuePairing}
      >
        <span>Continue</span>
        <svg viewBox="0 0 20 20" aria-hidden="true">
          <path d="m7.5 4.5 5.5 5.5-5.5 5.5" />
        </svg>
      </button>
    </div>

    {#if submissionNote}
      <p class="submission-note" role="status">{submissionNote}</p>
    {/if}
    </section>
  {/if}

  <footer class="privacy-note">
    <svg viewBox="0 0 18 18" aria-hidden="true">
      <rect x="3.5" y="7.5" width="11" height="8" rx="2" />
      <path d="M6 7.5V5.75a3 3 0 0 1 6 0V7.5" />
    </svg>
    {#if startupState === "macfuse-missing"}
      <span>No quicKFS connection was started.</span>
    {:else if startupState === "checking"}
      <span>Checking local requirements…</span>
    {:else}
      <span>Pairing codes are never written to browser storage.</span>
    {/if}
    {#if bootstrap && startupState === "ready"}
      <span class="backend-state" title={`Read limit: ${bootstrap.maxClientReadSize} bytes`}>
        {bootstrap.platform} client ready
      </span>
    {/if}
  </footer>
</main>
