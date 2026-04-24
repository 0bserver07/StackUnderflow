export default function LoadingSpinner({
  size = 'md',
  message,
}: {
  size?: 'sm' | 'md' | 'lg'
  message?: string
}) {
  const sizeClasses = { sm: 'h-4 w-4', md: 'h-8 w-8', lg: 'h-12 w-12' }
  return (
    <div className="flex flex-col items-center justify-center p-4">
      <div
        className={`${sizeClasses[size]} animate-spin rounded-full border-2 border-gray-300 dark:border-gray-700 border-t-blue-500`}
      />
      {message && <p className="mt-3 text-sm text-gray-600 dark:text-gray-400">{message}</p>}
    </div>
  )
}
