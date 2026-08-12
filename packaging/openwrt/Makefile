include $(TOPDIR)/rules.mk

PKG_NAME:=qin
PKG_VERSION:=0.2.0
PKG_RELEASE:=1
PKG_LICENSE:=Apache-2.0

include $(INCLUDE_DIR)/package.mk

define Package/qin
  SECTION:=utils
  CATEGORY:=Utilities
  TITLE:=Command-line AI agent
  DEPENDS:=+ca-bundle
endef

define Package/qin/description
 qin is a one-shot command-line AI agent with sessions and local knowledge search.
endef

# The release pipeline must place the target-specific static binary here.
define Build/Compile
endef

define Package/qin/install
	$(INSTALL_DIR) $(1)/usr/bin
	$(INSTALL_BIN) ./files/qin $(1)/usr/bin/qin
	$(INSTALL_DIR) $(1)/etc/qin
	$(INSTALL_CONF) ./files/config.toml.example $(1)/etc/qin/config.toml.example
endef

$(eval $(call BuildPackage,qin))
