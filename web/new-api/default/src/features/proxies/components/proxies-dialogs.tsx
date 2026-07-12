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
import { ProxiesDeleteDialog } from './proxies-delete-dialog'
import { ProxiesMutateDrawer } from './proxies-mutate-drawer'
import { useProxies } from './proxies-provider'

export function ProxiesDialogs() {
  const { open, setOpen, currentRow } = useProxies()
  const isUpdate = open === 'update'
  const showMutate = open === 'create' || isUpdate
  const showDelete = open === 'delete'

  return (
    <>
      {showMutate && (
        <ProxiesMutateDrawer
          open={showMutate}
          onOpenChange={(isOpen) => !isOpen && setOpen(null)}
          currentRow={isUpdate ? currentRow || undefined : undefined}
        />
      )}
      {showDelete && <ProxiesDeleteDialog />}
    </>
  )
}
