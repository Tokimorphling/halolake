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
import { useQuery } from '@tanstack/react-query'
import { useState } from 'react'
import { useTranslation } from 'react-i18next'
import { toast } from 'sonner'

import {
  DISABLED_ROW_DESKTOP,
  DISABLED_ROW_MOBILE,
  DataTablePage,
  useDataTable,
} from '@/components/data-table'

import { listProxies } from '../api'
import { ERROR_MESSAGES, PROXY_STATUS } from '../constants'
import type { Proxy } from '../types'
import { useProxiesColumns } from './proxies-columns'
import { useProxies } from './proxies-provider'

function isDisabledProxyRow(proxy: Proxy) {
  return proxy.status !== PROXY_STATUS.ENABLED
}

export function ProxiesTable() {
  const { t } = useTranslation()
  const columns = useProxiesColumns()
  const { refreshTrigger } = useProxies()
  const [globalFilter, setGlobalFilter] = useState('')

  const { data, isLoading, isFetching } = useQuery({
    queryKey: ['proxies', refreshTrigger],
    queryFn: async () => {
      const result = await listProxies()
      if (!result.success) {
        toast.error(result.message || t(ERROR_MESSAGES.LOAD_FAILED))
        return [] as Proxy[]
      }
      return result.data || []
    },
    placeholderData: (previousData) => previousData,
  })

  const proxies = data || []

  const { table } = useDataTable({
    data: proxies,
    columns,
    globalFilter,
    onGlobalFilterChange: setGlobalFilter,
    globalFilterFn: (row, _columnId, filterValue) => {
      const name = String(row.getValue('name')).toLowerCase()
      const url = String(row.getValue('url')).toLowerCase()
      const id = String(row.getValue('id'))
      const searchValue = String(filterValue).toLowerCase()
      return (
        name.includes(searchValue) ||
        url.includes(searchValue) ||
        id.includes(searchValue)
      )
    },
  })

  return (
    <DataTablePage
      table={table}
      columns={columns}
      isLoading={isLoading}
      isFetching={isFetching}
      emptyTitle={t('No Proxies Found')}
      emptyDescription={t(
        'No upstream proxies configured. Create a proxy to assign it to channels.'
      )}
      skeletonKeyPrefix='proxies-skeleton'
      applyHeaderSize
      toolbarProps={{
        searchPlaceholder: t('Filter by name, URL, or ID...'),
      }}
      getRowClassName={(row, { isMobile }) => {
        if (!isDisabledProxyRow(row.original)) return undefined
        return isMobile ? DISABLED_ROW_MOBILE : DISABLED_ROW_DESKTOP
      }}
    />
  )
}
