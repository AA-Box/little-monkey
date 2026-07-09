export type Theme = 'light' | 'dark'

const STORAGE_KEY = 'little-monkey-theme'

export function getStoredTheme(): Theme {
  return (localStorage.getItem(STORAGE_KEY) as Theme) === 'dark' ? 'dark' : 'light'
}

export function applyTheme(theme: Theme): void {
  document.documentElement.setAttribute('data-theme', theme)
  localStorage.setItem(STORAGE_KEY, theme)
}
