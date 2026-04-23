import { IconMoon, IconSun } from '@tabler/icons-react'
import { useTheme } from '../../hooks/useTheme'

interface ThemeToggleProps {
  className?: string
}

/**
 * Small button that flips the active theme.
 *
 * Shows the icon representing the *next* state — i.e. in dark mode we render
 * the sun (click = "switch to light"), and in light mode we render the moon.
 */
export default function ThemeToggle({ className }: ThemeToggleProps) {
  const { theme, toggle } = useTheme()
  const nextTheme = theme === 'dark' ? 'light' : 'dark'
  const Icon = theme === 'dark' ? IconSun : IconMoon
  const label = `Switch to ${nextTheme} mode`

  const baseClass =
    'inline-flex items-center justify-center rounded-md p-1.5 text-gray-600 dark:text-gray-400 ' +
    'hover:bg-gray-100 dark:hover:bg-gray-800 hover:text-gray-900 dark:hover:text-gray-100 focus:outline-none focus-visible:ring-2 ' +
    'focus-visible:ring-blue-500 transition-colors'
  const mergedClass = className ? `${baseClass} ${className}` : baseClass

  return (
    <button
      type="button"
      onClick={toggle}
      className={mergedClass}
      aria-label={label}
      title={label}
    >
      <Icon size={18} stroke={1.75} aria-hidden="true" />
    </button>
  )
}
