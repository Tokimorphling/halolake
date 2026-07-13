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

import { SectionPageLayout } from '@/components/layout'

import { ProxiesDialogs } from './components/proxies-dialogs'
import { ProxiesPrimaryButtons } from './components/proxies-primary-buttons'
import { ProxiesProvider } from './components/proxies-provider'
import { ProxiesTable } from './components/proxies-table'

export function Proxies() {
  const { t } = useTranslation()
  return (
    <ProxiesProvider>
      <SectionPageLayout fixedContent>
        <SectionPageLayout.Title>
          <span className='block min-w-0'>
            <span className='block truncate'>{t('Proxies')}</span>
            <span className='text-muted-foreground mt-0.5 block truncate text-xs font-normal tracking-normal sm:text-sm'>
              {t('Upstream egress proxies for channel traffic')}
            </span>
          </span>
        </SectionPageLayout.Title>
        <SectionPageLayout.Actions>
          <ProxiesPrimaryButtons />
        </SectionPageLayout.Actions>
        <SectionPageLayout.Content>
          <ProxiesTable />
        </SectionPageLayout.Content>
      </SectionPageLayout>
      <ProxiesDialogs />
    </ProxiesProvider>
  )
}
