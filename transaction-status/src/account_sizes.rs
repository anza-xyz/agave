pub type TransactionAccountSizes = Vec<Vec<usize>>;

#[derive(Debug)]
pub struct TransactionAccountSizesSet {
    pub pre_acc_sizes: TransactionAccountSizes,
    pub post_acc_sizes: TransactionAccountSizes,
}

impl TransactionAccountSizesSet {
    pub fn new(
        pre_acc_sizes: TransactionAccountSizes,
        post_acc_sizes: TransactionAccountSizes,
    ) -> Self {
        assert_eq!(pre_acc_sizes.len(), post_acc_sizes.len());
        Self {
            pre_acc_sizes,
            post_acc_sizes,
        }
    }
}
