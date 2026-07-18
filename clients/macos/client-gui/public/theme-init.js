(() => {
  const stored = localStorage.getItem("quickfs-theme");
  const preference = ["system", "light", "dark"].includes(stored) ? stored : "system";
  const systemDark = window.matchMedia("(prefers-color-scheme: dark)").matches;
  const effective = preference === "system" ? (systemDark ? "dark" : "light") : preference;

  document.documentElement.dataset.themePreference = preference;
  document.documentElement.dataset.theme = effective;
  document
    .querySelector('meta[name="theme-color"]')
    ?.setAttribute("content", effective === "dark" ? "#242321" : "#f5fffb");
})();
