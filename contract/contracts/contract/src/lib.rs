#![no_std]

// Import các macro + kiểu dữ liệu cần cho hợp đồng Soroban.
use soroban_sdk::{
    contract, contracterror, contractevent, contractimpl, contracttype, symbol_short, Address, Bytes,
    Env, Map, Symbol, Vec,
};
use soroban_sdk::token::StellarAssetClient;

// =====================
// Errors (lỗi trả về cho contract)
// =====================
#[contracterror]
#[derive(Clone, Copy, Eq, PartialEq)]
pub enum Error {
    // Người gọi không đủ quyền (seller/buyer không khớp hoặc chưa ký auth)
    BadAuth = 1,

    // Không tìm thấy dataset
    DatasetNotFound = 2,

    // Không tìm thấy purchase/escrow
    PurchNotFound = 3,

    // Dataset đã tồn tại (upload trùng id)
    DatasetAlreadyExists = 4,

    // Số tiền ký quỹ không hợp lệ (khác giá hoặc <= 0)
    BadAmount = 5,

    // Chỉ seller mới xác nhận
    NotSeller = 6,

    // Trạng thái purchase không cho phép confirm (ví dụ đã fulfilled)
    BadStatus = 7,

    // Token contract (Stellar Asset) ở lúc confirm không khớp token đã escrow
    TokenMismatch = 8,

    // amount vượt quá i128::MAX (vì token transfer dùng i128)
    AmountTooLarge = 9,
}

// =====================
// Storage types
// =====================

// Dataset: dữ liệu có thể được bán (chỉ lưu mô tả + seller + giá).
#[contracttype]
#[derive(Clone)]
pub struct Dataset {
    pub seller: Address, // Tài khoản bán
    pub dname: Bytes,   // Tên / nhãn dữ liệu (off-chain do bạn tự quản)
    pub ddesc: Bytes,   // Mô tả dữ liệu (off-chain)
    pub price: u128,    // Giá bán (số lượng Stellar Asset)
}

// Trạng thái escrow/buy.
#[contracttype]
#[derive(Clone)]
pub enum PStatus {
    Pending,  // Buyer đã escrow, chờ seller confirm
    Done,     // Seller confirm, contract đã chuyển tiền
}

// Purchase/escrow: ghi nhận buyer đã ký quỹ và chờ confirm.
#[contracttype]
#[derive(Clone)]
pub struct Purchase {
    pub did: u32,      // dataset id
    pub buyer: Address,// người mua
    pub amt: u128,     // số tiền đã escrow
    pub tok: Address,  // địa chỉ Stellar Asset contract dùng để chuyển tiền
    pub st: PStatus,   // trạng thái
    pub rel_seq: u32,  // ledger sequence khi fulfilled (để debug/index)
}

// =====================
// Events (tuỳ chọn, hữu ích để bạn log / index)
// =====================

#[contractevent]
#[derive(Clone)]
pub struct EvUp {
    #[topic]
    pub did: u32,
    pub seller: Address,
    pub price: u128,
}

#[contractevent]
#[derive(Clone)]
pub struct EvEsc {
    #[topic]
    pub did: u32,
    #[topic]
    pub pid: u64,
    pub buyer: Address,
    pub amt: u128,
}

#[contractevent]
#[derive(Clone)]
pub struct EvPay {
    #[topic]
    pub did: u32,
    #[topic]
    pub pid: u64,
    pub seller: Address,
    pub buyer: Address,
    pub amt: u128,
    pub rel_seq: u32,
}

// =====================
// Contract
// =====================
#[contract]
pub struct DataMarket;

#[contractimpl]
impl DataMarket {
    // Các key lưu storage (symbol_short bắt buộc <= 9 ký tự).
    fn ds_key() -> Symbol {
        symbol_short!("DS")
    }
    fn pu_key() -> Symbol {
        symbol_short!("PU")
    }
    fn np_key() -> Symbol {
        symbol_short!("NP") // next purchase id
    }

    // Helper: chuyển u128 -> i128 cho StellarAssetClient transfer.
    fn u128_to_i128(e: &Env, amt: u128) -> Result<i128, Error> {
        let _ = e; // chỉ để tránh unused (nếu bạn không cần env ở đây)
        if amt > i128::MAX as u128 {
            return Err(Error::AmountTooLarge);
        }
        Ok(amt as i128)
    }

    // =====================
    // init
    // =====================
    pub fn init(env: Env) {
        // Khởi tạo bộ đếm purchase id = 0.
        env.storage()
            .persistent()
            .set(&Self::np_key(), &0u64);
    }

    // =====================
    // upload_data (seller đăng bán)
    // =====================
    pub fn upload_data(
        env: Env,
        seller: Address, // địa chỉ seller (bạn truyền vào từ client)
        did: u32,        // dataset id do bạn tự chọn
        dname: Bytes,    // tên nhãn (off-chain)
        ddesc: Bytes,    // mô tả (off-chain)
        price: u128,     // giá bán
    ) -> Result<(), Error> {
        // Xác thực: seller phải ký auth cho lời gọi này.
        seller.require_auth();

        // Load map dataset đang có.
        let mut ds: Map<u32, Dataset> = env
            .storage()
            .persistent()
            .get(&Self::ds_key())
            .unwrap_or_else(|| Map::new(&env));

        // Dataset id đã tồn tại => lỗi.
        if ds.get(did).is_some() {
            return Err(Error::DatasetAlreadyExists);
        }

        // Tạo dataset mới.
        let item = Dataset {
            seller,
            dname,
            ddesc,
            price,
        };

        // Lưu dataset vào storage.
        ds.set(did, item);
        env.storage()
            .persistent()
            .set(&Self::ds_key(), &ds);

        // Phát event để debug.
        let seller_addr = ds.get(did).unwrap().seller.clone();
        EvUp {
            did,
            seller: seller_addr,
            price,
        }
        .publish(&env);

        Ok(())
    }

    // =====================
    // escrow_purchase (buyer ký quỹ + hợp đồng nhận tiền)
    // =====================
    pub fn escrow_purchase(
        env: Env,
        buyer: Address,  // người mua (truyền vào)
        did: u32,         // dataset id
        amt: u128,        // số tiền escrow (phải == price)
        tok: Address,     // địa chỉ StellarAsset contract (asset để chuyển)
    ) -> Result<u64, Error> {
        // Buyer phải ký auth.
        buyer.require_auth();

        // Load dataset map.
        let ds: Map<u32, Dataset> = env
            .storage()
            .persistent()
            .get(&Self::ds_key())
            .unwrap_or_else(|| Map::new(&env));

        // Tìm dataset.
        let item = ds.get(did).ok_or(Error::DatasetNotFound)?;

        // Kiểm tra số tiền escrow phải đúng giá (đơn giản hoá).
        if amt == 0 || amt != item.price {
            return Err(Error::BadAmount);
        }

        // Lấy purchase id tiếp theo.
        let pid: u64 = env
            .storage()
            .persistent()
            .get(&Self::np_key())
            .unwrap_or(0u64);

        // Tăng purchase id.
        env.storage()
            .persistent()
            .set(&Self::np_key(), &(pid + 1));

        // Chuyển token từ buyer vào chính hợp đồng (escrow thực).
        //
        // Lưu ý quan trọng:
        // - Bạn phải truyền `tok` là Stellar Asset contract address (hoặc token phù hợp interface).
        // - Lời gọi này sẽ yêu cầu buyer đã có auth cho require_auth bên token contract.
        let asset = StellarAssetClient::new(&env, &tok);
        let amt_i128 = Self::u128_to_i128(&env, amt)?;
        let escrow_addr = env.current_contract_address();

        asset.transfer(&buyer, &escrow_addr, &amt_i128);

        // Load purchases map.
        let mut pu: Map<u64, Purchase> = env
            .storage()
            .persistent()
            .get(&Self::pu_key())
            .unwrap_or_else(|| Map::new(&env));

        // Ghi purchase trạng thái Pending.
        pu.set(
            pid,
            Purchase {
                did,
                buyer: buyer.clone(),
                amt,
                tok: tok.clone(),
                st: PStatus::Pending,
                rel_seq: 0u32,
            },
        );

        // Lưu purchases map.
        env.storage().persistent().set(&Self::pu_key(), &pu);

        // Phát event escrow.
        EvEsc {
            did,
            pid,
            buyer,
            amt,
        }
        .publish(&env);

        Ok(pid)
    }

    // =====================
    // confirm_delivery (seller xác nhận đã nhận dữ liệu -> contract chuyển tiền)
    // =====================
    pub fn confirm_delivery(
        env: Env,
        seller: Address,  // seller xác nhận (truyền vào)
        pid: u64,         // purchase id
        tok: Address,     // token contract (phải khớp với purchase.tok)
    ) -> Result<(), Error> {
        // Seller phải ký auth.
        seller.require_auth();

        // Load purchases map.
        let mut pu: Map<u64, Purchase> = env
            .storage()
            .persistent()
            .get(&Self::pu_key())
            .unwrap_or_else(|| Map::new(&env));

        // Lấy purchase.
        let mut p = pu.get(pid).ok_or(Error::PurchNotFound)?;

        // Kiểm tra token mismatch.
        if p.tok != tok {
            return Err(Error::TokenMismatch);
        }

        // Nếu không pending => lỗi.
        match p.st {
            PStatus::Pending => {}
            _ => return Err(Error::BadStatus),
        }

        // Load dataset để check seller.
        let ds: Map<u32, Dataset> = env
            .storage()
            .persistent()
            .get(&Self::ds_key())
            .unwrap_or_else(|| Map::new(&env));

        let item = ds.get(p.did).ok_or(Error::DatasetNotFound)?;

        // Chỉ seller của dataset mới được confirm.
        if item.seller != seller {
            return Err(Error::NotSeller);
        }

        // Chuyển tiền từ contract -> seller.
        let asset = StellarAssetClient::new(&env, &tok);
        let amt_i128 = Self::u128_to_i128(&env, p.amt)?;
        let escrow_addr = env.current_contract_address();

        asset.transfer(&escrow_addr, &seller, &amt_i128);

        // Cập nhật trạng thái fulfilled.
        p.st = PStatus::Done;
        p.rel_seq = env.ledger().sequence();

        // Lưu lại purchase.
        pu.set(pid, p.clone());
        env.storage().persistent().set(&Self::pu_key(), &pu);

        // Phát event thanh toán.
        EvPay {
            did: p.did,
            pid,
            seller,
            buyer: p.buyer,
            amt: p.amt,
            rel_seq: p.rel_seq,
        }
        .publish(&env);

        Ok(())
    }

    // =====================
    // View: get_purchase (phục vụ test)
    // =====================
    pub fn get_purchase(env: Env, pid: u64) -> Result<Purchase, Error> {
        let pu: Map<u64, Purchase> = env
            .storage()
            .persistent()
            .get(&Self::pu_key())
            .unwrap_or_else(|| Map::new(&env));

        pu.get(pid).ok_or(Error::PurchNotFound)
    }
}