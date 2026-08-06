/** @type {import('tailwindcss').Config} */
export default {
  darkMode: 'class',
  content: [
    "./index.html",
    "./src/**/*.{js,ts,jsx,tsx}",
  ],
  theme: {
    extend: {
      colors: {
        // Remap blue to match the backend's indigo-purple accent (#667eea / #764ba2)
        blue: {
          50:  '#f0f1fe',
          100: '#e0e4fd',
          200: '#c1c8fb',
          300: '#a2adf9',
          400: '#8494ef',
          500: '#667eea',
          600: '#5a6fd8',
          700: '#4a5ab8',
          800: '#3b4798',
          900: '#2c3578',
          950: '#1e2452',
        },
      },
    },
  },
  plugins: [require('@tailwindcss/typography')],
}
