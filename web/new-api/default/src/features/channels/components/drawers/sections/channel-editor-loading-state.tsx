/*
Copyright (C) 2023-2026 QuantumNous

This program is free software: you can redistribute it and/or modify
it under the terms of the GNU Affero General Public License as
published by the Free Software Foundation, either version 3 of the
License, or (at your option) any later version.

This program is distributed in the hope that it will be useful,
but WITHOUT ANY WARRANTY; without even the implied warranty of
MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
GNU Affero General Public License for more details.

You should have received a copy of the GNU Affero General Public License
along with this program. If not, see <https://www.gnu.org/licenses/>.

For commercial licensing, please contact support@quantumnous.com
*/
import { useTranslation } from 'react-i18next'

import { Skeleton } from '@/components/ui/skeleton'

export function ChannelEditorLoadingState() {
  const { t } = useTranslation()

  return (
    <div
      className='bg-card/70 ring-foreground/6 flex flex-col gap-4 rounded-2xl p-4 shadow-[0_1px_0_0_var(--hairline)] ring-1 sm:p-5'
      aria-live='polite'
    >
      <div>
        <p className='text-title text-sm font-semibold tracking-[var(--tracking-title)]'>
          {t('Loading channel details')}
        </p>
        <p className='text-muted-foreground mt-1.5 text-xs leading-relaxed'>
          {t('Please wait before editing to avoid overwriting saved values.')}
        </p>
      </div>
      <div className='grid gap-4 sm:grid-cols-2'>
        <Skeleton className='h-10 w-full rounded-lg' />
        <Skeleton className='h-10 w-full rounded-lg' />
      </div>
      <Skeleton className='h-24 w-full rounded-xl' />
      <Skeleton className='h-32 w-full rounded-xl' />
    </div>
  )
}
